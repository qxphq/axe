//! The REPL and session lifecycle: provider setup, the line loop, slash
//! commands, the agent turn, and the transcript formatting.

mod api;
mod complete;
mod config;
mod editor;
mod http;
mod markdown;
mod mcp;
mod prompt;
mod render;
mod repl;
mod sandbox;
mod sessions;
mod skills;
mod startup;
mod state;
mod store;
mod text;
mod tools;
mod turn;
mod ui;
mod web;

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use serde_json::{json, Value};

use prompt::mode_note;
use render::{
    body, call_line, detail, failed, line_of, mark, plain_text, shell_echo, shell_line, Paint,
};
use repl::{
    choose, narrow, pump, readline, recall_lines, show_queue, spawn_input, take_back, Event, Input,
    Pending,
};
use sessions::COMPACT_NOTE;
use startup::{
    dir_label, find, fullness, level, level_names, models, percent, place, scaled, session_id,
    set_thinking, thinking_label, NO_SESSIONS,
};
use store::{history_bytes, weight, Live, Store};
use text::{head_tail, strip_control, terminal_text};
use turn::{PlainSink, Queue, Sink, TextSink};

// ---- providers and the command line ----

#[derive(Clone)]
struct Provider {
    name: String,
    cfg: config::ProviderConfig,
    ep: http::Endpoint,
    key: Option<String>,
    headers: Vec<(String, String)>,
    models: Vec<api::ModelInfo>,
}

/// What the arguments ask for. A flag that answers a question is decided before
/// the config, the network or the terminal: none of the three can contribute to
/// it, and all three can fail.
#[derive(Debug, PartialEq)]
enum Cli {
    Run,
    Version,
    Help,
    /// An argument axe does not take. Ignored, it would look like a setting
    /// that did nothing; named, it is the usage line.
    Unknown(String),
}

fn cli(mut args: impl Iterator<Item = String>) -> Cli {
    match args.next() {
        None => Cli::Run,
        Some(arg) => match arg.as_str() {
            "--version" | "-V" => Cli::Version,
            "--help" | "-h" => Cli::Help,
            _ => Cli::Unknown(arg),
        },
    }
}

/// `--help`: what the binary does with and without a terminal, and where the
/// config lives. The manual is `README.md`, which `about` hands the model.
fn usage() -> String {
    format!(
        "usage: axe [--version | --help]

  axe            interactive session in the working directory
  axe < prompts  one request per line; stdout is the model's text
  --version      print the version and exit
  --help         print this and exit

config: {}
commands: /help inside a session; the model reads the README with `about`
",
        config::path().display()
    )
}

fn main() {
    match cli(std::env::args().skip(1)) {
        // Answered before anything else: no config, no network, no terminal.
        Cli::Version => {
            println!("axe {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Cli::Help => {
            print!("{}", usage());
            return;
        }
        Cli::Unknown(arg) => {
            eprintln!("axe: unknown argument: {}", strip_control(&arg));
            eprint!("\n{}", usage());
            std::process::exit(2);
        }
        Cli::Run => {}
    }
    let result = startup::run();
    // MCP servers are ours: nothing outlives the session that started it. They
    // go first — a server still alive has the scratch in its write rules and
    // could put a file back under it.
    mcp::shutdown();
    // The scratch directory is the one thing a session creates outside the
    // terminal; a static's `Drop` never runs, so this is the hook.
    sandbox::cleanup();
    if let Err(e) = result {
        eprintln!("axe: {}", strip_control(&e));
        std::process::exit(1);
    }
}

/// The first request and the latest answer, short enough to name from. Naming
/// waits for an assistant message: a session that only ran `!cmd` gets the
/// fallback.
fn naming_excerpt(messages: &[Value]) -> Option<(String, String)> {
    if !messages
        .iter()
        .any(|message| message["role"].as_str() == Some("assistant"))
    {
        return None;
    }
    let user = messages
        .iter()
        .filter(|message| message["role"].as_str() == Some("user"))
        .filter_map(|message| message["content"].as_str())
        .find(|text| {
            !text.starts_with("explore mode:")
                && !text.starts_with('$')
                && !text.starts_with(COMPACT_NOTE)
        })?;
    let assistant = messages
        .iter()
        .filter(|message| message["role"].as_str() == Some("assistant"))
        .filter_map(|message| message["content"].as_str())
        .rfind(|text| !text.trim().is_empty())
        .unwrap_or("");
    Some((
        sessions::collapse(user, 1500),
        sessions::collapse(assistant, 1500),
    ))
}

/// `/compact`'s instruction: the reply is the summary and nothing else.
const COMPACT_PROMPT: &str = "You summarize a coding session so it can continue in a smaller \
context. Reply with the summary only, no preamble. Keep every path, command, identifier, error \
message and decision exact. Sections: task, what has been done, files changed, commands run, \
decisions and constraints, open questions, next step. Under 800 words.";

/// One tool-less completion for `/compact`. The summary is text, not a turn.
///
/// No `max_tokens`: a cap small enough for a summary of 800 words is small
/// enough for a thinking model to spend on reasoning before it writes a word,
/// and then the answer that arrives is empty. The prompt sets the length;
/// `finish_reason` is what tells a cut-off reply from a silent one.
fn compact_request(
    provider: &Provider,
    model: &str,
    level: Option<&str>,
    messages: &[Value],
    cancel: &http::Cancel,
) -> Result<api::Reply, String> {
    let extra = tool_less_extra(provider, level, 0.0);
    api::stream_chat(
        &provider.ep,
        provider.key.as_deref(),
        &provider.headers,
        model,
        messages,
        &json!([]),
        Some(&extra),
        cancel,
        |_| {},
        |_| {},
    )
}

/// The body of a request that has no tools and no turn: the session's thinking
/// level, so the provider does not think on terms of its own, and the
/// temperature the caller wants.
fn tool_less_extra(provider: &Provider, level: Option<&str>, temperature: f64) -> Value {
    let mut extra = provider
        .cfg
        .thinking_body(level)
        .unwrap_or_else(|| json!({}));
    if let Some(body) = extra.as_object_mut() {
        body.insert("temperature".into(), json!(temperature));
    }
    extra
}

/// What a reply that carried no text did instead, for the one place axe asks
/// for text it cannot show: `/compact`. The finish reason is the difference
/// between an answer cut off and a model that said nothing.
fn no_text(reply: &api::Reply) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(reason) = &reply.finish_reason {
        parts.push(format!("finish reason: {reason}"));
    }
    if let Some(usage) = &reply.usage {
        parts.push(format!("{} completion tokens", usage.completion));
    }
    if !reply.reasoning.trim().is_empty() {
        parts.push(format!(
            "{} chars of reasoning",
            reply.reasoning.chars().count()
        ));
    }
    if parts.is_empty() {
        "the provider sent no text".to_string()
    } else {
        parts.join(", ")
    }
}

/// The conversation as one plain-text transcript for the summarizing request.
/// API messages would carry `tool_calls` without the `tools` array that defines
/// them, which a strict provider refuses; text cannot. The oldest messages go
/// first when the transcript does not fit `budget`, and the newest always stays.
fn compaction_prompt(messages: &[Value], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages.iter().skip(2) {
        let role = message["role"].as_str().unwrap_or("");
        if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
            let (head, tail) = if role == "tool" {
                (800, 200)
            } else {
                (2000, 500)
            };
            lines.push(format!("{role}: {}", head_tail(text, head, tail)));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args = call["function"]["arguments"].as_str().unwrap_or("");
                lines.push(format!(
                    "{role}: calls {name} {}",
                    head_tail(args, 300, 100)
                ));
            }
        }
    }
    let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut dropped = 0;
    while total > budget && lines.len() > 1 {
        total -= lines[0].len() + 1;
        lines.remove(0);
        dropped += 1;
    }
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!("[{dropped} earlier messages omitted]\n"));
    }
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One small completion, without tools. The session header stays: providers
/// that require it would refuse the request otherwise. Like `/compact` it sends
/// no `max_tokens`, so a thinking model cannot spend the answer's budget.
fn title_request(
    provider: &Provider,
    model: &str,
    level: Option<&str>,
    user: &str,
    assistant: &str,
) -> Option<String> {
    let messages = vec![
        json!({
            "role": "system",
            "content": "You name conversations. Reply with exactly two lines:\n\
                        TITLE: a descriptive title of three to six words\n\
                        SUMMARY: one sentence describing the task, under 25 words\n\
                        No quotes, no markdown, no extra lines."
        }),
        json!({
            "role": "user",
            "content": format!("User: {user}\nAssistant: {assistant}"),
        }),
    ];
    let extra = tool_less_extra(provider, level, 0.3);
    let cancel = http::Cancel::new();
    api::stream_chat(
        &provider.ep,
        provider.key.as_deref(),
        &provider.headers,
        model,
        &messages,
        &json!([]),
        Some(&extra),
        &cancel,
        |_| {},
        |_| {},
    )
    .ok()
    .map(|reply| reply.text)
}

/// The two lines the naming prompt asks for. Anything else falls back.
fn parse_naming(text: &str) -> Option<(String, String)> {
    let mut title = None;
    let mut summary = None;
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim();
        if value.is_empty() {
            continue;
        }
        if key.trim().eq_ignore_ascii_case("title") {
            title = Some(value.to_string());
        } else if key.trim().eq_ignore_ascii_case("summary") {
            summary = Some(value.to_string());
        }
    }
    title.map(|title| (title, summary.unwrap_or_default()))
}

/// Everything the REPL mutates between turns: the model selection and history.
struct Session {
    /// What this directory remembers. Held rather than re-read so a `/model`,
    /// `/thinking` or `/thoughts` change is a write, not a reload.
    state: state::State,
    providers: Vec<Provider>,
    provider: usize,
    model: String,
    thinking: Option<String>,
    /// Explore mode: `read`, the web tools and a confined `bash` only
    /// (`/explore`). Starts from `cfg.explore_by_default`; the history carries
    /// the note.
    mode: tools::Mode,
    /// The last round's token count, from the provider. `None` until a turn
    /// reports one: the fill is then estimated from the history.
    used: Option<u64>,
    messages: Vec<Value>,
    /// The history's own size in bytes, kept as it is appended to: the request
    /// is serialized once per message rather than once per info row.
    bytes: u64,
    /// The session id: also the provider's `session_header` value.
    sid: String,
    /// The append-only body and its sidecar, when there is a working directory
    /// to keep them in.
    store: Option<Store>,
    /// The directory's recent sessions, newest first, for the startup chooser.
    startup: Vec<sessions::Meta>,
    /// The ids a piped `/sessions` listing numbered, for `/sessions N`.
    picks: Vec<String>,
    /// A background title request, when one is in flight.
    naming: Option<Receiver<Option<(String, String)>>>,
    /// A user-facing warning to render at the next safe point.
    warning: Option<String>,
    /// The skills discovered at startup; the catalogue is in the prompt.
    skills: Vec<skills::Skill>,
}

fn session_title(meta: &sessions::Meta) -> String {
    if !meta.title.is_empty() {
        meta.title.clone()
    } else if !meta.summary.is_empty() {
        sessions::collapse(&meta.summary, 60)
    } else {
        "(untitled)".to_string()
    }
}

/// One listing row: date, title, size, and whether the body is still packed.
fn session_row(meta: &sessions::Meta) -> String {
    let mut row = format!(
        "{}  {}  ·  {} msgs",
        sessions::stamp(meta.updated),
        session_title(meta),
        meta.messages
    );
    if meta.archived {
        row.push_str("  ·  zip");
    }
    row
}

/// This directory's sessions that match `query`, as (id, row). An empty query
/// keeps them all, newest first.
fn session_rows(
    dir: &std::path::Path,
    metas: &[sessions::Meta],
    query: &str,
) -> Vec<(String, String)> {
    sessions::search(dir, metas, query)
        .into_iter()
        .map(|hit| {
            let meta = &metas[hit.index];
            let mut row = session_row(meta);
            if !hit.snippet.is_empty() {
                row.push_str("  ·  ");
                row.push_str(&hit.snippet);
            }
            (meta.id.clone(), row)
        })
        .collect()
}

// ---- commands ----

/// What a submitted line asks for.
enum Cmd {
    Exit,
    /// Handled already, or nothing to do.
    Continue,
    /// Send this to the model.
    Turn(String),
    /// Replace the history with a summary of it, then carry on.
    Compact,
    /// Run this locally (`!cmd`); the output joins the history without a turn.
    Shell(String),
}

/// A parsed prompt line. Both dispatchers share this so `/model`, `/thinking`
/// and `/sessions` cannot drift in spelling or ordering.
#[derive(Debug, PartialEq)]
enum Command {
    Empty,
    Exit,
    Help,
    New,
    /// None lists and opens the picker; Some is a `provider/id` or bare model id.
    /// The scope says whose selection it becomes: this directory's, or the
    /// default every directory without one starts on.
    Model(Option<String>, Scope),
    /// None prints the current level; Some sets it.
    Thinking(Option<String>),
    Thoughts,
    Explore,
    /// `/mcp [add|remove|forget|name]`: the servers and their tools.
    Mcp(String),
    /// A chooser in the interactive run, a numbered list when piped.
    Sessions(Option<String>),
    /// Replace the history with a summary of itself.
    Compact,
    Skills,
    /// `/skill:name request`: the body becomes the user turn.
    Skill(String, String),
    Shell(String),
    Turn(String),
    Unknown(String),
}

/// Whose selection a `/model` line becomes.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Scope {
    /// This directory's state file.
    Dir,
    /// The state root's `default.json`, which a directory with no pin follows.
    Global,
}

/// Split `/model`'s argument into a scope and the rest. `-g`/`--global` is a
/// flag only as a whole word, so a model id that starts with it stays a model.
fn model_arg(rest: &str) -> (Scope, Option<String>) {
    let rest = rest.trim();
    for flag in ["--global", "-g"] {
        if let Some(after) = rest.strip_prefix(flag) {
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                let spec = after.trim();
                return (
                    Scope::Global,
                    if spec.is_empty() {
                        None
                    } else {
                        Some(spec.to_string())
                    },
                );
            }
        }
    }
    (
        Scope::Dir,
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        },
    )
}

fn parse_command(line: &str) -> Command {
    let input = line.trim();
    if input.is_empty() {
        return Command::Empty;
    }
    match input {
        "/exit" | "/quit" => return Command::Exit,
        "/help" => return Command::Help,
        "/new" => return Command::New,
        "/compact" => return Command::Compact,
        "/model" => return Command::Model(None, Scope::Dir),
        "/thinking" => return Command::Thinking(None),
        "/thoughts" => return Command::Thoughts,
        "/explore" => return Command::Explore,
        "/mcp" => return Command::Mcp(String::new()),
        "/sessions" => return Command::Sessions(None),
        "/skills" => return Command::Skills,
        _ => {}
    }
    if let Some(rest) = input.strip_prefix("/model ") {
        let (scope, spec) = model_arg(rest);
        return Command::Model(spec, scope);
    }
    if let Some(rest) = input.strip_prefix("/mcp ") {
        return Command::Mcp(rest.trim().to_string());
    }
    if let Some(rest) = input.strip_prefix("/thinking ") {
        return Command::Thinking(Some(rest.trim().to_string()));
    }
    if let Some(rest) = input.strip_prefix("/sessions ") {
        return Command::Sessions(Some(rest.trim().to_string()));
    }
    if let Some(rest) = input.strip_prefix("/skill:") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Command::Unknown(input.to_string());
        }
        let (name, request) = match rest.split_once(char::is_whitespace) {
            Some((name, request)) => (name, request.trim()),
            None => (rest, ""),
        };
        return Command::Skill(name.to_string(), request.to_string());
    }
    if let Some(rest) = input.strip_prefix('!') {
        let command = rest.trim();
        return if command.is_empty() {
            Command::Empty
        } else {
            Command::Shell(command.to_string())
        };
    }
    if input.starts_with('/') {
        return Command::Unknown(input.to_string());
    }
    Command::Turn(input.to_string())
}

/// Slash commands, for Tab completion in the prompt. Only the name is
/// completed; `/model` and `/thinking` take their argument as typed.
const COMMANDS: &[&str] = &[
    "/compact",
    "/exit",
    "/explore",
    "/help",
    "/mcp",
    "/model",
    "/new",
    "/quit",
    "/sessions",
    "/skills",
    "/thinking",
    "/thoughts",
];

/// A skill name that can be typed as one word after `/skill:`.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

const HELP: &str = "/model [id]    list the models, or switch to one for this directory\n\
                    /model -g [id] set the default a directory without one starts on\n\
                    /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                    /thoughts      show or hide the model's reasoning\n\
                    /sessions [q]  list, search and resume sessions\n\
                    /compact       replace the history with a summary of itself\n\
                    /skills        list loaded skills (project, global, config)\n\
                    /mcp [name]    MCP servers: list, add, remove, forget\n\
                    /skill:name [request]  load a skill's instructions and run a turn\n\
                    /new           start a new session\n\
                    /explore       toggle explore mode (read-only shell)\n\
                    !cmd           run a shell command; its output joins the context\n\
                    /help          list the commands, the keys and the config\n\
                    /exit, /quit   quit\n\
                    \n\
                    Tab            complete a command or a path token\n\
                    ↑ ↓            walk the candidates, then this directory's history\n\
                    ↑ with a queue take the waiting lines back to the prompt\n\
                    Ctrl+R         search that history\n\
                    Shift+Tab      toggle explore mode\n\
                    Esc            close a chooser; twice, stop the running turn\n\
                    Ctrl+J, Alt+Enter  new line\n\
                    Ctrl+D         end input\n";

/// `/help`: the command list, then where the config is read from and the keys
/// it takes. The path is resolved rather than written out, because
/// `XDG_CONFIG_HOME` moves it.
fn help_text() -> String {
    format!(
        "{HELP}\nconfig: {}\n\
         config keys: providers, model, explore_by_default, search, thinking, system, agents, skills\n\
         mcp: mcp.json is global, <repo>/.mcp.json is per project (/mcp)\n\
         provider keys: base_url, api_key_env, models_url, session_header, thinking",
        config::path().display()
    )
}

// ---- the session ----

/// What the sandbox said about itself at startup, when it is weaker than the
/// mode note promises. The terminal hears it once; the history carries it too,
/// so the model is never told a confinement the kernel is not enforcing.
static SANDBOX_NOTE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Keep the startup line for the histories built later: `resume`, `/new` and
/// `/compact` all rebuild from `base_history`.
pub(crate) fn note_sandbox(text: Option<String>) {
    if let Some(text) = text {
        let _ = SANDBOX_NOTE.set(text);
    }
}

/// The messages a history starts with: the built system prompt, the current
/// mode note, and the sandbox's own caveat when it has one.
pub(crate) fn base_history(system: Value, mode: tools::Mode, sandbox: Option<&str>) -> Vec<Value> {
    let mut messages = vec![system, mode_note(mode)];
    if let Some(note) = sandbox {
        messages.push(json!({ "role": "user", "content": note }));
    }
    messages
}

/// Whether the line loop is done: nothing a finished turn handed back, and no
/// input thread left to send another line.
fn done(pending: &VecDeque<String>, input_ended: bool) -> bool {
    pending.is_empty() && input_ended
}

impl Session {
    /// One line at a time from a terminal we own. Keys arrive on the channel, so
    /// a turn in flight never blocks the prompt.
    fn run_interactive(mut self) -> Result<(), String> {
        let mut screen = ui::Screen::new().map_err(|e| format!("cannot take the terminal: {e}"))?;
        screen.set_explore(self.mode.explore());
        screen.set_commands(self.commands());
        screen.set_info(self.info());
        screen.set_history(state::read_history(&self.state.cwd));
        screen.set_completer(complete::paths);
        // The directory's recent sessions are a chooser, not a printout: the
        // list can be walked with the arrows and vanishes when typing starts.
        if !self.startup.is_empty() {
            let rows = self.startup.iter().map(session_row).collect();
            screen.set_menu(
                "Resume a session (↑↓ · Enter), or type to start a new one",
                rows,
                true,
            );
        }
        let (tx, rx) = mpsc::channel();
        let input = spawn_input(tx.clone());

        // A line submitted mid-turn arrives here when the turn could not take
        // it: a command, which needs the session and the screen, or a request
        // that missed every round boundary. It is dispatched exactly like a
        // line typed while idle, which is why slash commands are handled in
        // this loop and not only where they are parsed.
        let mut pending: VecDeque<String> = VecDeque::new();
        loop {
            self.poll_naming(Some(&mut screen));
            let (line, was_queued) = match pending.pop_front() {
                Some(line) => (line, true),
                // A line a finished turn handed back is still the user's; the
                // prompt is only reached while input is still there, and a
                // turn or a command that consumed the `Eof` event must not
                // leave the loop waiting for a key no thread can send.
                None if done(&pending, input.load(Ordering::SeqCst)) => break,
                None => {
                    // The title request lands while this loop blocks on keys:
                    // the idle tick applies it and repaints the row.
                    let mut idle = |screen: &mut ui::Screen| {
                        self.poll_naming(Some(screen));
                    };
                    match readline(&mut screen, &rx, &mut idle) {
                        Some(Input::Line(line)) => {
                            // Remembered before it is dispatched, so a command
                            // is a step like a request.
                            if let Err(e) = state::append_history(&self.state.cwd, &line) {
                                self.warn(format!("history: {e}"));
                            }
                            (line, false)
                        }
                        Some(Input::Explore) => {
                            self.toggle_explore(&mut Console::Screen {
                                screen: &mut screen,
                                rx: &rx,
                            });
                            continue;
                        }
                        Some(Input::Menu(index)) => {
                            let Some(meta) = self.startup.get(index).cloned() else {
                                continue;
                            };
                            screen.clear_menu(false);
                            self.startup.clear();
                            if let Err(e) = self.resume(&meta.id, Some(&mut screen)) {
                                screen.error(&format!("axe: {e}\n"));
                            }
                            continue;
                        }
                        Some(Input::Cancel) => {
                            self.startup.clear();
                            continue;
                        }
                        None => break,
                    }
                }
            };
            // A committed line is echoed by `readline`; a queued one was not.
            if was_queued {
                screen.echo_user(&line);
            }
            match self.dispatch(
                &line,
                &mut Console::Screen {
                    screen: &mut screen,
                    rx: &rx,
                },
            ) {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    pending.extend(self.messages_turn(&input, &tx, &rx, &mut screen)?);
                }
                Cmd::Shell(command) => pending.extend(self.shell(&command, &mut screen, &rx)),
                Cmd::Compact => pending.extend(self.compact(&mut Console::Screen {
                    screen: &mut screen,
                    rx: &rx,
                })),
            }
            self.flush_warnings(Some(&mut screen));
        }
        self.finalize_session();
        self.flush_warnings(Some(&mut screen));
        screen.close();
        Ok(())
    }

    /// No tty: the kernel edits the line and the turn prints as it streams.
    /// Nothing is printed to ask for a line — the input is a pipe, and stdout
    /// belongs to the answers.
    fn run_piped(mut self) -> Result<(), String> {
        // Whether the answer is the run's first block: it then starts at the
        // top of stdout. Any other line may have printed, and closes the
        // question — a command, a `!cmd`, a blank line no one can see.
        let mut opening = true;
        while let Some(line) = ui::cooked_line() {
            let cmd = match line.trim() {
                "" => Cmd::Continue,
                _ => self.dispatch(&line, &mut Console::Stdout),
            };
            let turning = matches!(&cmd, Cmd::Turn(_));
            match cmd {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    let mut sink = if std::mem::take(&mut opening) {
                        PlainSink::flush_top()
                    } else {
                        PlainSink::default()
                    };
                    let cancel = http::Cancel::new();
                    self.record(json!({ "role": "user", "content": input }));
                    if let Err(e) = self.turn(&mut sink, &cancel) {
                        eprintln!("axe: {}", strip_control(&e));
                    }
                    self.name_session();
                }
                Cmd::Shell(command) => {
                    // The user's own shell, and the same line and glyph the
                    // screen draws: `!cmd` is never confined, so the line is
                    // the unconfined color.
                    let mut console = Console::Stdout;
                    let result = self.run_shell(&command);
                    console.output(&format!("{}\n", plain_text(&shell_echo(&command, &result))));
                    console.output(body(&result));
                }
                Cmd::Compact => {
                    self.compact(&mut Console::Stdout);
                }
            }
            if !turning {
                opening = false;
            }
            self.flush_warnings(None);
        }
        self.finalize_session();
        self.flush_warnings(None);
        Ok(())
    }

    /// Persist a message and add it to the history. Mode notes stay out: the
    /// mode is state, and resume writes the current one.
    fn record(&mut self, message: Value) {
        if let Some(store) = self.store.as_mut() {
            store.record(&message);
        }
        self.bytes += weight(&message);
        self.messages.push(message);
    }

    fn warn(&mut self, text: String) {
        if self.warning.is_none() {
            self.warning = Some(text);
        }
    }

    /// Render the first pending warning at a safe point: the screen when there
    /// is one, stderr when the run is piped.
    fn flush_warnings(&mut self, screen: Option<&mut ui::Screen>) {
        if let Some(text) = self.store.as_mut().and_then(|store| store.take_warning()) {
            self.warn(text);
        }
        let Some(text) = self.warning.take() else {
            return;
        };
        match screen {
            Some(screen) => screen.error(&format!("axe: {text}\n")),
            None => eprintln!("axe: {}", strip_control(&text)),
        }
    }

    fn spec(&self) -> String {
        format!("{}/{}", self.providers[self.provider].name, self.model)
    }

    /// Switch model: reset the fill count, remember the level for the new
    /// `provider/id` (state first, then config), persist the selection under
    /// `scope`, and return the new info row.
    fn set_model(&mut self, provider: usize, id: String, scope: Scope) -> Vec<(ui::Info, String)> {
        self.provider = provider;
        self.model = id;
        // The count belonged to the old model's window.
        self.used = None;
        let spec = self.spec();
        self.thinking = self.state.thinking_for(&spec).or_else(|| {
            self.providers[provider]
                .cfg
                .thinking
                .iter()
                .find(|(name, _)| name == &spec)
                .map(|(level, _)| level.clone())
        });
        match scope {
            Scope::Dir => {
                self.state.set_model(&spec);
                if let Err(e) = self.state.save() {
                    self.warn(format!("state: {e}"));
                }
            }
            // A global default only reaches a directory that pins no model, so
            // this one's pin goes: otherwise the new default would never apply
            // here and the command would read as a no-op.
            Scope::Global => match state::save_default(&spec) {
                Err(e) => self.warn(e),
                Ok(()) => {
                    if self.state.model.take().is_some() {
                        if let Err(e) = self.state.save() {
                            self.warn(format!("state: {e}"));
                        }
                    }
                }
            },
        }
        self.info()
    }

    /// Apply a model choice and say so: the info row, and the line the
    /// transcript keeps.
    fn choose_model(&mut self, provider: usize, id: String, scope: Scope, console: &mut Console) {
        let info = self.set_model(provider, id, scope);
        console.output(&format!("{}\n", ui::info_text(&info)));
        console.info(info);
        if scope == Scope::Global {
            console.output(&format!(
                "◦ Default model: {} (a directory without one starts here)\n",
                self.spec()
            ));
        }
    }

    /// Apply a thinking level and remember it for this `provider/id`.
    fn choose_level(&mut self, level: String, console: &mut Console) {
        self.thinking = Some(level.clone());
        console.info(self.info());
        let spec = self.spec();
        self.state.set_thinking(&spec, level.as_str());
        if let Err(e) = self.state.save() {
            self.warn(format!("state: {e}"));
        }
        let label = thinking_label(&self.providers[self.provider].cfg, &self.thinking);
        console.output(&format!("Thinking: {label}\n"));
    }

    fn finalize_session(&mut self) {
        if let Some(store) = self.store.as_mut() {
            store.finalize(&self.messages);
        }
    }

    /// The provider's affinity token follows the conversation. A resumed
    /// session must keep its own, or the provider starts a cold one.
    fn set_session_header(&mut self) {
        let sid = self.sid.clone();
        for provider in &mut self.providers {
            let Some(name) = provider.cfg.session_header.clone() else {
                continue;
            };
            let mut found = false;
            for (key, value) in &mut provider.headers {
                if *key == name {
                    *value = sid.clone();
                    found = true;
                }
            }
            if !found {
                provider.headers.push((name, sid.clone()));
            }
        }
    }

    /// Load a stored session into this one. The current system prompt stays:
    /// instructions are current, history is history.
    fn resume(&mut self, id: &str, screen: Option<&mut ui::Screen>) -> Result<String, String> {
        let dir = self
            .store
            .as_ref()
            .map(|store| store.dir.clone())
            .ok_or_else(|| "no working directory to keep sessions in".to_string())?;
        let mut meta = sessions::Meta::load(&dir, id)?;
        let mut warnings = Vec::new();
        let messages = sessions::load_messages(&dir, &mut meta, &mut warnings)?;
        // Selecting a session is activity: it moves to the top of the listing
        // and is not re-archived on the next start. A read-only state directory
        // must not make the history unreadable.
        meta.updated = sessions::now();
        if let Err(e) = meta.save(&dir) {
            warnings.push(format!("session meta: {e}"));
        }
        let title = session_title(&meta);
        let store = Store::resumed(dir, meta)?;

        self.messages = self.base_messages();
        self.messages.extend(messages);
        self.bytes = history_bytes(&self.messages);
        self.sid = id.to_string();
        self.store = Some(store);
        self.naming = None;
        self.used = None;
        self.set_session_header();

        if let Some(screen) = screen {
            for warning in warnings {
                screen.error(&format!("axe: {warning}\n"));
            }
            screen.output(&format!("◦ Resumed: {title}\n"));
            self.replay(screen);
            screen.set_info(self.info());
        } else {
            for warning in warnings {
                eprintln!("axe: {}", strip_control(&warning));
            }
        }
        Ok(title)
    }

    /// Put the loaded conversation back on screen. Only the tail is drawn: the
    /// model has all of it, and a long session must not bury the prompt in
    /// scrollback. Tool calls carry their recorded outcome.
    fn replay(&self, screen: &mut ui::Screen) {
        let history = &self.messages[2..];
        let mut results: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for message in history {
            if message["role"].as_str() != Some("tool") {
                continue;
            }
            if let (Some(id), Some(content)) = (
                message["tool_call_id"].as_str(),
                message["content"].as_str(),
            ) {
                results.insert(id, content);
            }
        }

        let start = history.len().saturating_sub(100);
        if start > 0 {
            screen.output(&format!("◦ {start} earlier messages\n"));
        }
        for message in &history[start..] {
            match message["role"].as_str() {
                Some("user") => {
                    if let Some(text) = message["content"].as_str() {
                        // The compaction summary is the session's own record
                        // of what it carries, not something the user said. It
                        // was printed when it was made, under the notice that
                        // records it; a resume starts from it without printing
                        // it again.
                        if !text.starts_with(COMPACT_NOTE) {
                            screen.echo_user(text);
                        }
                    }
                }
                Some("assistant") => {
                    if self.state.thoughts {
                        if let Some(text) = message["reasoning_content"].as_str() {
                            screen.thought(text);
                        }
                    }
                    if let Some(text) = message["content"].as_str() {
                        if !text.trim().is_empty() {
                            screen.text(text);
                            screen.flush();
                        }
                    }
                    let Some(calls) = message["tool_calls"].as_array() else {
                        continue;
                    };
                    for call in calls {
                        let call = api::ToolCall {
                            id: call["id"].as_str().unwrap_or("").to_string(),
                            name: call["function"]["name"].as_str().unwrap_or("").to_string(),
                            arguments: call["function"]["arguments"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                        };
                        let result = results.get(call.id.as_str()).copied().unwrap_or("");
                        let mut spans = call_line(&call, self.mode.explore());
                        spans.push((Paint::Plain, " ".to_string()));
                        let (paint, glyph) = mark(&call.name, result);
                        spans.push((paint, glyph.to_string()));
                        screen.insert(vec![line_of(&spans)], ui::Block::Tool);
                        if failed(&call.name, result) {
                            screen.fail(&detail(result));
                        }
                    }
                }
                _ => {}
            }
        }
        screen.set_status("");
    }

    /// `/new`: finalize what is open, then start with a clean history. The id
    /// and the provider token change together.
    fn new_session(&mut self, screen: Option<&mut ui::Screen>) {
        self.finalize_session();
        let sid = session_id();
        let dir = self.store.as_ref().map(|store| store.dir.clone());
        if let Some(dir) = dir {
            self.store = Some(Store::fresh(dir, &sid));
        }
        self.messages = self.base_messages();
        self.bytes = history_bytes(&self.messages);
        self.sid = sid;
        self.naming = None;
        self.picks.clear();
        self.used = None;
        self.set_session_header();
        if let Some(screen) = screen {
            screen.reset();
            screen.set_info(self.info());
        }
    }

    /// A generated title is attempted once, after the first answer exists.
    fn may_name(&self) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        let Some(meta) = &store.meta else {
            return false;
        };
        meta.title.is_empty() && !store.naming_tried && naming_excerpt(&self.messages).is_some()
    }

    /// Start the title request off the UI thread. The prompt stays live; the
    /// result is applied at the next idle poll. Piped runs keep the synchronous
    /// `name_session` because they have no UI to block.
    fn start_naming(&mut self) {
        if !self.may_name() {
            return;
        }
        let Some((user, assistant)) = naming_excerpt(&self.messages) else {
            return;
        };
        if let Some(store) = self.store.as_mut() {
            store.naming_tried = true;
        }
        let provider = self.providers[self.provider].clone();
        let model = self.model.clone();
        let level = self.thinking.clone();
        let fallback = sessions::fallback(&self.messages);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let named = title_request(&provider, &model, level.as_deref(), &user, &assistant)
                .as_deref()
                .and_then(parse_naming)
                .unwrap_or(fallback);
            let _ = tx.send(Some(named));
        });
        self.naming = Some(rx);
    }

    /// Apply a finished background title, if one is ready. A title is metadata;
    /// waiting for the next key is enough and never blocks the prompt.
    fn poll_naming(&mut self, mut screen: Option<&mut ui::Screen>) {
        let Some(received) = self.naming.as_ref().map(|rx| rx.try_recv()) else {
            return;
        };
        match received {
            Ok(Some((title, summary))) => {
                self.naming = None;
                if !title.is_empty() {
                    if let Some(store) = self.store.as_mut() {
                        store.set_title(&title, &summary);
                    }
                }
                // The title is part of the selection row: marking it is
                // enough, the idle tick repaints.
                if let Some(screen) = screen.as_deref_mut() {
                    screen.set_info(self.info());
                }
                self.flush_warnings(screen);
            }
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.naming = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn name_session(&mut self) {
        if !self.may_name() {
            return;
        }
        let Some((user, assistant)) = naming_excerpt(&self.messages) else {
            return;
        };
        if let Some(store) = self.store.as_mut() {
            store.naming_tried = true;
        }
        let model = self.model.clone();
        let provider = &self.providers[self.provider];
        let named = title_request(
            provider,
            &model,
            self.thinking.as_deref(),
            &user,
            &assistant,
        )
        .as_deref()
        .and_then(parse_naming);
        let (title, summary) = named.unwrap_or_else(|| sessions::fallback(&self.messages));
        if title.is_empty() {
            return;
        }
        if let Some(store) = self.store.as_mut() {
            store.set_title(&title, &summary);
        }
    }

    /// `/sessions`: a chooser over this directory's sessions. Typing becomes a
    /// search, Enter on an empty line resumes the selected row, Esc closes.
    fn browse_sessions(&mut self, screen: &mut ui::Screen, rx: &Receiver<Event>, query: &str) {
        if self.state.cwd.as_os_str().is_empty() {
            screen.error("axe: sessions need a working directory\n");
            return;
        }
        let metas = sessions::list(&self.state.cwd);
        if metas.is_empty() {
            screen.output(NO_SESSIONS);
            return;
        }
        let dir = self.state.cwd.clone();
        let Some(id) = choose(screen, rx, "Sessions", "search", query, None, |query| {
            session_rows(&dir, &metas, query)
        }) else {
            return;
        };
        if let Err(e) = self.resume(&id, Some(screen)) {
            screen.error(&format!("axe: {e}\n"));
        }
    }

    /// `/model`: a chooser over every provider's models, opened on the one in
    /// use. Typing narrows the list, Enter switches, Esc closes. The pick
    /// becomes the selection `scope` names.
    fn browse_models(
        &mut self,
        screen: &mut ui::Screen,
        rx: &Receiver<Event>,
        query: &str,
        scope: Scope,
    ) {
        let items: Vec<(String, String)> = self
            .providers
            .iter()
            .flat_map(|provider| {
                provider.models.iter().map(move |model| {
                    let spec = format!("{}/{}", provider.name, model.id);
                    let row = match model.context {
                        Some(ctx) => format!("{spec}  ({ctx} ctx)"),
                        None => spec.clone(),
                    };
                    (spec, row)
                })
            })
            .collect();
        let header = match scope {
            Scope::Dir => "Models",
            Scope::Global => "Default model",
        };
        let on = self.spec();
        let Some(picked) = choose(screen, rx, header, "narrow", query, Some(&on), |query| {
            narrow(&items, query)
        }) else {
            return;
        };
        let Some((provider, id)) = find(&self.providers, &picked) else {
            return;
        };
        let mut console = Console::Screen { screen, rx };
        self.choose_model(provider, id, scope, &mut console);
    }

    /// `/thinking`: a chooser over the levels this provider configures. A
    /// provider with one level has nothing to pick and never gets here.
    fn browse_levels(&mut self, screen: &mut ui::Screen, rx: &Receiver<Event>, query: &str) {
        let names = level_names(&self.providers[self.provider].cfg);
        let items: Vec<(String, String)> = names
            .iter()
            .map(|name| (name.clone(), name.clone()))
            .collect();
        let on = self.thinking.clone();
        let Some(level) = choose(
            screen,
            rx,
            "Thinking levels",
            "narrow",
            query,
            on.as_deref(),
            |query| narrow(&items, query),
        ) else {
            return;
        };
        let mut console = Console::Screen { screen, rx };
        self.choose_level(level, &mut console);
    }

    /// The no-tty form: a numbered list, and a number to resume. A search
    /// replaces the list, so the numbers always refer to what was last shown.
    fn list_piped(&mut self, arg: &str) {
        if self.state.cwd.as_os_str().is_empty() {
            eprintln!("axe: sessions need a working directory");
            return;
        }
        if let Ok(number) = arg.parse::<usize>() {
            let Some(id) = number
                .checked_sub(1)
                .and_then(|at| self.picks.get(at))
                .cloned()
            else {
                eprintln!("axe: no session #{number}; run /sessions first");
                return;
            };
            match self.resume(&id, None) {
                Ok(title) => println!("◦ Resumed: {}", terminal_text(&title)),
                Err(e) => eprintln!("axe: {}", strip_control(&e)),
            }
            return;
        }

        let all = sessions::list(&self.state.cwd);
        if all.is_empty() {
            print!("{NO_SESSIONS}");
            self.picks.clear();
            return;
        }
        let selected = session_rows(&self.state.cwd, &all, arg);
        if selected.is_empty() {
            println!("no sessions match {:?}", arg.trim());
            self.picks.clear();
            return;
        }
        self.picks = selected.iter().map(|(id, _)| id.clone()).collect();
        for (at, (_, row)) in selected.iter().enumerate() {
            println!("{:>2}) {}", at + 1, terminal_text(row));
        }
        // The numbers are useless without the line that says what to do with
        // one, and a run without a terminal cannot arrow its way out.
        println!("resume with /sessions <n>, or search with /sessions <query>");
    }

    /// The live region's last row: what this session is pointed at, how much of
    /// the window it has filled, and the mode when that changes the tool set.
    /// One piece per thing, so the screen can color each on its own.
    fn info(&self) -> Vec<(ui::Info, String)> {
        let mut parts = Vec::new();
        if let Some(title) = self.title() {
            parts.push((ui::Info::Title, title));
        }
        // Where it runs, then what it talks to: the directory is the session's
        // own setting and sits left of the model it names. A session with no
        // working directory has nothing to say here.
        let dir = dir_label(&self.state.cwd);
        if !dir.is_empty() {
            parts.push((ui::Info::Dir, dir));
        }
        parts.push((
            ui::Info::Selection,
            place(&self.providers, self.provider, &self.model),
        ));
        parts.push((ui::Info::Level, level(&self.thinking)));
        let (fill, fullness) = self.fill();
        parts.push((ui::Info::Fill(fullness), fill));
        if self.mode.explore() {
            parts.push((ui::Info::Mode, "read-only".to_string()));
        }
        parts
    }

    /// The name on the selection row: the generated title, or, until the model
    /// has named the session, the first request — what `finalize` would write at
    /// the end. It rides in the right group, left of the model, so the transient
    /// status keeps its own left half.
    fn title(&self) -> Option<String> {
        if let Some(meta) = self.store.as_ref().and_then(|store| store.meta.as_ref()) {
            let title = meta.title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
        // Live, not written: the sidecar keeps the generated title's place.
        let (provisional, _) = sessions::fallback(&self.messages);
        Some(provisional).filter(|title| !title.is_empty())
    }

    /// Used and total tokens, with the percentage between them. The provider's
    /// own count when the last turn reported one, a character estimate
    /// otherwise; no percentage when the model does not say how big its window
    /// is. The second value is how full the window is, the gauge the info row
    /// colors: without a count there is nothing to measure.
    fn fill(&self) -> (String, Option<u8>) {
        let used = self.used.unwrap_or(self.bytes / 4);
        match self.context() {
            Some(ctx) => (
                format!("{}/{} ({})", scaled(used), scaled(ctx), percent(used, ctx)),
                Some(fullness(used, ctx)),
            ),
            None => (scaled(used), None),
        }
    }

    fn context(&self) -> Option<u64> {
        let models = &self.providers[self.provider].models;
        models.iter().find(|m| m.id == self.model)?.context
    }

    /// Set the mode. The prompt and the tool catalogue are fixed, so the mode
    /// is announced in the history: the model reads the note, and nothing
    /// before it is rewritten.
    fn set_mode(&mut self, mode: tools::Mode) {
        self.mode = mode;
        let note = mode_note(mode);
        self.bytes += weight(&note);
        self.messages.push(note);
    }

    /// `/explore` and Shift+Tab share one path, so the prompt's box, the
    /// selection row and the transcript never disagree about the mode.
    fn toggle_explore(&mut self, console: &mut Console) {
        self.set_mode(self.mode.flip());
        console.explore(self.mode.explore());
        console.info(self.info());
        console.output(&format!(
            "Explore: {}\n",
            if self.mode.explore() { "on" } else { "off" }
        ));
        // A running server cannot be narrowed or widened, so flipping the mode
        // restarts them under the new rules. The tool names stay what they are.
        for line in mcp::restart(self.mode.explore()) {
            console.output(&format!("{line}\n"));
        }
    }

    /// The messages every history starts with: the built system prompt, the
    /// current mode note, and the sandbox's caveat when it has one. The prefix
    /// itself is never rewritten.
    fn base_messages(&self) -> Vec<Value> {
        let system = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| json!({ "role": "system", "content": tools::system_prompt() }));
        base_history(system, self.mode, SANDBOX_NOTE.get().map(String::as_str))
    }

    /// The lines Tab completes: the slash commands, and `/skill:<name>` for
    /// every loaded skill whose name can be typed as one word. No argument is
    /// in the list: `/model` and `/thinking` take theirs from a chooser, and a
    /// candidate would be taken by Enter before the command ran.
    fn commands(&self) -> Vec<String> {
        let mut commands: Vec<String> =
            COMMANDS.iter().map(|command| command.to_string()).collect();
        for skill in self.skills.iter().filter(|skill| safe_name(&skill.name)) {
            commands.push(format!("/skill:{}", skill.name));
        }
        commands
    }

    /// `/skills`: what was found, where it came from, and what the model sees.
    fn skill_listing(&self) -> String {
        let mut out = String::new();
        for skill in &self.skills {
            out.push_str(&format!(
                "{}  ({}, {})\n  {}\n  {}\n",
                skill.name,
                skill.scope.label(),
                if skill.hidden { "hidden" } else { "visible" },
                skill.file.display(),
                skill.description
            ));
            if !skill.allowed_tools.is_empty() {
                out.push_str(&format!("  tools: {}\n", skill.allowed_tools.join(" ")));
            }
        }
        if out.is_empty() {
            // The empty list is the one place that has to say how a skill is
            // made, where axe looks, and that the start is when it is read.
            out.push_str(&format!(
                "no skills\n  \
                 project  .agents/skills/<name>/SKILL.md, in the repository\n  \
                 global   {}/skills/ and ~/.agents/skills/<name>/SKILL.md\n  \
                 name and description load at startup, the body on /skill:<name>\n",
                config::path()
                    .parent()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_else(|| ".".to_string())
            ));
        }
        // Names, descriptions and paths come from a repository: a terminal
        // must not interpret them.
        strip_control(&out)
    }

    /// `!cmd`: run it here, show it, and let the output join the history. No
    /// turn follows — the model sees it when the user next asks something.
    fn shell(
        &mut self,
        command: &str,
        screen: &mut ui::Screen,
        rx: &Receiver<Event>,
    ) -> Option<String> {
        // What is running, while it runs: the finished line appears below once
        // there is a result to put on it, the same way a tool call does.
        screen.set_status(&plain_text(&shell_line(command, false)));
        screen.repaint();
        let cancel = http::Cancel::new();
        let args = json!({ "command": command }).to_string();
        let mut queued: Option<String> = None;
        let result = thread::scope(|scope| {
            // `!cmd` is the user's own shell: explore mode does not confine
            // it, so the mode here is only about which tool set is offered.
            let handle =
                scope.spawn(|| tools::execute("bash", &args, &cancel, tools::Mode::Normal));
            let (joined, line) = pump(screen, rx, &cancel, handle);
            queued = line;
            joined.unwrap_or_else(|| "error: shell thread panicked".into())
        });
        screen.set_status("");
        screen.set_alert(None);
        screen.insert(
            vec![line_of(&shell_echo(command, &result))],
            ui::Block::Tool,
        );
        // The command's own words: the glyph on the line has already said how
        // it went, so the `exit: N` header would only repeat it.
        if failed("bash", &result) {
            screen.fail(body(&result));
        } else {
            screen.output(body(&result));
        }
        // The output is part of the history now: drop the provider's last
        // count so the fill estimate includes it.
        self.used = None;
        self.record(json!({
            "role": "user",
            "content": format!("$ {command}\n{result}"),
        }));
        screen.set_info(self.info());
        screen.repaint();
        queued
    }

    /// `/compact`: ask the model to summarize the session, then continue from
    /// the summary. The body is not rewritten — the summary is appended as a
    /// marked user message and the loader starts replay from it. It is printed
    /// once, under the notice, because it is what the model now carries.
    ///
    /// A line submitted while the request runs comes back like `shell`'s: the
    /// prompt stays live, and nothing typed is lost.
    fn compact(&mut self, console: &mut Console) -> Option<String> {
        if !self
            .messages
            .iter()
            .skip(2)
            .any(|message| message["role"].as_str() == Some("assistant"))
        {
            console.output("◦ nothing to compact\n");
            return None;
        }
        // Half the window, as characters: the request has to fit with room for
        // the answer. 50 000 tokens is the most `/compact` will send.
        let budget = self
            .context()
            .map(|window| {
                usize::try_from(window)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(2)
            })
            .unwrap_or(200_000)
            .min(200_000);
        let prompt = compaction_prompt(&self.messages, budget);
        let request = vec![
            json!({ "role": "system", "content": COMPACT_PROMPT }),
            json!({ "role": "user", "content": prompt }),
        ];
        let cancel = http::Cancel::new();
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let level = self.thinking.clone();
        let mut queued = None;
        let result = match console {
            Console::Screen { screen, rx } => {
                screen.set_status("▣ Compacting");
                screen.repaint();
                let result = thread::scope(|scope| {
                    let handle = scope.spawn(|| {
                        compact_request(provider, &model, level.as_deref(), &request, &cancel)
                    });
                    let (joined, line) = pump(screen, rx, &cancel, handle);
                    queued = line;
                    joined.unwrap_or_else(|| Err("compaction thread panicked".into()))
                });
                screen.set_status("");
                screen.set_alert(None);
                result
            }
            Console::Stdout => {
                compact_request(provider, &model, level.as_deref(), &request, &cancel)
            }
        };

        match result {
            // A summary the provider cut is only part of the session: the
            // history stays as it was, and the line says why. It is the same
            // test a turn's answer gets.
            Ok(reply) if reply.cut().is_some() => console.error(&format!(
                "compaction: the summary was cut ({}); the session is unchanged",
                reply.cut().unwrap_or_default()
            )),
            Ok(reply) if !reply.text.trim().is_empty() => {
                let summary = reply.text.trim().to_string();
                let replaced = self.messages.len().saturating_sub(2) as u64;
                let content = format!("{COMPACT_NOTE}\n\n{summary}");
                self.messages = self.base_messages();
                self.messages
                    .push(json!({ "role": "user", "content": content }));
                self.bytes = history_bytes(&self.messages);
                if let Some(store) = self.store.as_mut() {
                    store.compacted(&sessions::compacted(&content, replaced));
                }
                // The stored count was the old request's; the next reply sets it
                // again, and the estimate stands in until then.
                self.used = None;
                if let Some(screen) = console.screen() {
                    screen.set_info(self.info());
                }
                console.output(&format!("◦ compacted {replaced} messages into 1 summary\n"));
                console.text(&summary);
            }
            Ok(reply) => console.error(&format!(
                "compaction: the model returned nothing ({})",
                no_text(&reply)
            )),
            Err(_) if cancel.cancelled() => console.output("× Compaction cancelled\n"),
            Err(e) => console.error(&e),
        }
        if let Some(screen) = console.screen() {
            screen.repaint();
        }
        queued
    }

    /// The command and its output as a user message. The `!` is the prompt's
    /// syntax, not the model's, so the message reads as a shell transcript.
    fn run_shell(&mut self, command: &str) -> String {
        let cancel = http::Cancel::new();
        let result = tools::execute(
            "bash",
            &json!({ "command": command }).to_string(),
            &cancel,
            tools::Mode::Normal,
        );
        self.record(json!({
            "role": "user",
            "content": format!("$ {command}\n{result}"),
        }));
        result
    }

    /// Run one turn on its own thread while this one keeps drawing.
    fn messages_turn(
        &mut self,
        input: &str,
        tx: &Sender<Event>,
        rx: &Receiver<Event>,
        screen: &mut ui::Screen,
    ) -> Result<Vec<String>, String> {
        self.record(json!({ "role": "user", "content": input }));
        // The first request is the session's name until the model gives it one:
        // show it now, not once the turn is over.
        screen.set_info(self.info());

        let cancel = http::Cancel::new();
        // A line typed while this turn runs, handed to the turn's own thread so
        // that it joins the history at the next round boundary instead of after
        // the last one. A command cannot go that way and waits for the end.
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let turn_queue = queue.clone();
        // Everything the live prompt owns: the waiting lines, the interrupt,
        // and the unfinished line. The turn's thread owns the history.
        let mut held = Held::default();

        screen.set_status("⋯ Waiting for provider");
        screen.repaint();

        // What the last round counted, handed back by the turn's thread. The
        // history is borrowed by that thread, so `self` is out of reach until
        // the scope ends.
        let mut used: Option<u64> = None;
        let mut live = Live {
            messages: &mut self.messages,
            bytes: &mut self.bytes,
            store: self.store.as_mut(),
        };
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let extra = provider.cfg.thinking_body(self.thinking.as_deref());
        let thoughts = self.state.thoughts;
        let mode = self.mode;
        let turn_tx = tx.clone();
        let turn_cancel = &cancel;

        let result = thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let mut sink = ChannelSink {
                    tx: turn_tx.clone(),
                    out: TextSink::default(),
                    thoughts,
                    thought: None,
                };
                let turn = turn::Turn {
                    ep: &provider.ep,
                    key: provider.key.as_deref(),
                    headers: &provider.headers,
                    model: &model,
                    extra: extra.as_ref(),
                    mode,
                    queue: Some(&turn_queue),
                    cancel: turn_cancel,
                };
                let result = turn::run_turn(&turn, &mut live, &mut sink);
                let _ = turn_tx.send(Event::Done(result));
            });

            loop {
                // Block for the first event, then take everything already
                // queued before drawing once: a repaint per token is a waste.
                let first = match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(ev) => Some(ev),
                    Err(mpsc::RecvTimeoutError::Timeout) => match rx.try_recv() {
                        // The turn may have sent Done just before finishing;
                        // process that event rather than dropping it.
                        Ok(ev) => Some(ev),
                        Err(mpsc::TryRecvError::Empty) if handle.is_finished() => break,
                        Err(mpsc::TryRecvError::Empty) => None,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    },
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let Some(first) = first else {
                    continue;
                };
                let mut done = false;
                let mut next = Some(first);
                while let Some(ev) = next.take().or_else(|| rx.try_recv().ok()) {
                    // An interrupt does not break the loop: the turn thread
                    // still reports what the killed command did, and `Done`
                    // is the only event that ends the drain.
                    if held.event(ev, screen, &queue, &cancel, &mut used) {
                        done = true;
                        break;
                    }
                }
                // One paint per batch: the setters above marked the screen,
                // and a repaint with nothing marked is a no-op.
                screen.repaint();
                if done {
                    break;
                }
            }
            handle.join().is_ok()
        });
        screen.set_status("");
        screen.set_alert(None);
        // The transcript says the turn was stopped: an interrupted reply ends
        // mid-sentence and looks like a finished one otherwise.
        if held.interrupted {
            // No emoji-presentation glyph here: `unicode-width` calls those one
            // cell while the terminal may draw two, and the space after it is
            // the cost.
            screen.error("Turn interrupted — the reply is partial\n");
        }
        // A turn that reported nothing (interrupted, or a provider that does
        // not count) leaves the estimate in charge rather than a stale number.
        self.used = used;
        screen.set_info(self.info());

        // Held from the turn: applied now, so the note sits below the answer
        // and above whatever the user types next.
        if held.switch {
            self.toggle_explore(&mut Console::Screen {
                screen: &mut *screen,
                rx,
            });
        }

        // One small request names the session, once, off the UI thread. The
        // prompt is live immediately; the result is applied at the next poll.
        self.start_naming();
        self.flush_warnings(Some(screen));

        if !result {
            screen.error("axe: turn failed\n");
        }
        // A request typed while the turn's last round was already in flight
        // never reached a boundary: it is the next turn, like any other line
        // that arrived too late for one, and a command line never entered the
        // queue at all. The REPL runs them in the order they were typed.
        show_queue(screen, &[]);
        Ok(held.queued.into_iter().map(|entry| entry.line).collect())
    }

    fn turn(&mut self, sink: &mut dyn Sink, cancel: &http::Cancel) -> Result<(), String> {
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let extra = provider.cfg.thinking_body(self.thinking.as_deref());
        let turn = turn::Turn {
            ep: &provider.ep,
            key: provider.key.as_deref(),
            headers: &provider.headers,
            model: &model,
            extra: extra.as_ref(),
            mode: self.mode,
            queue: None,
            cancel,
        };
        let mut live = Live {
            messages: &mut self.messages,
            bytes: &mut self.bytes,
            store: self.store.as_mut(),
        };
        turn::run_turn(&turn, &mut live, sink).map(|_| ())
    }

    /// Slash commands and selection changes. The console is the only thing a
    /// command says anything to, so a terminal and a pipe run the same arms:
    /// the difference is where the text lands, not what it says.
    fn dispatch(&mut self, line: &str, console: &mut Console) -> Cmd {
        match parse_command(line) {
            Command::Empty => Cmd::Continue,
            Command::Exit => Cmd::Exit,
            Command::Help => {
                console.output(&help_text());
                Cmd::Continue
            }
            Command::New => {
                // Read before the reset: the screen is wiped at the boundary, so
                // this line is the only record of what was just saved.
                let saved = self.title().filter(|title| !title.trim().is_empty());
                self.new_session(console.screen());
                match saved {
                    Some(title) => console.output(&format!(
                        "◦ New session (saved: {})\n",
                        strip_control(&title)
                    )),
                    None => console.output("◦ New session\n"),
                }
                // A session begins read-only: `/new` is where one begins, so
                // the mode comes back with it. `toggle_explore` is the one
                // path that announces the change, colors the box, restarts
                // the servers and puts the note in the new history.
                if !self.mode.explore() {
                    self.toggle_explore(console);
                }
                Cmd::Continue
            }
            Command::Model(spec, scope) => {
                match spec {
                    None => match console {
                        Console::Screen { screen, rx } => self.browse_models(screen, rx, "", scope),
                        Console::Stdout => console.output(&models(&self.providers)),
                    },
                    Some(spec) => match find(&self.providers, &spec) {
                        Some((provider, id)) => self.choose_model(provider, id, scope, console),
                        // On a screen an argument that names no model is a
                        // search, not a mistake: the chooser opens narrowed to
                        // it.
                        None => match console {
                            Console::Screen { screen, rx } => {
                                self.browse_models(screen, rx, &spec, scope)
                            }
                            Console::Stdout => console.error(&format!("unknown model: {spec}")),
                        },
                    },
                }
                Cmd::Continue
            }
            Command::Thinking(rest) => {
                let input = rest.as_deref().unwrap_or("").trim().to_string();
                let label = thinking_label(&self.providers[self.provider].cfg, &self.thinking);
                let levels = level_names(&self.providers[self.provider].cfg);
                if input.is_empty() {
                    match console {
                        // One level is not a choice: the line says which it is.
                        Console::Screen { screen, rx } if levels.len() > 1 => {
                            self.browse_levels(screen, rx, "")
                        }
                        _ => console.output(&format!("Thinking: {label}\n")),
                    }
                    return Cmd::Continue;
                }
                match set_thinking(&self.providers[self.provider].cfg, &input) {
                    Ok(level) => self.choose_level(level, console),
                    Err(e) => console.error(&e),
                }
                Cmd::Continue
            }
            Command::Thoughts => {
                self.state.thoughts = !self.state.thoughts;
                if let Err(e) = self.state.save() {
                    self.warn(format!("state: {e}"));
                }
                console.output(&format!(
                    "Thoughts: {}\n",
                    if self.state.thoughts { "on" } else { "off" }
                ));
                Cmd::Continue
            }
            Command::Explore => {
                self.toggle_explore(console);
                Cmd::Continue
            }
            Command::Mcp(rest) => {
                console.output(&mcp::dispatch(&rest));
                Cmd::Continue
            }
            Command::Sessions(query) => {
                self.sessions(query.as_deref().unwrap_or(""), console);
                Cmd::Continue
            }
            Command::Compact => Cmd::Compact,
            Command::Skills => {
                console.output(&self.skill_listing());
                Cmd::Continue
            }
            Command::Skill(name, request) => {
                match self.skills.iter().find(|skill| skill.name == name) {
                    Some(skill) => match skills::invocation(skill, &request) {
                        Ok(text) => Cmd::Turn(text),
                        Err(e) => {
                            console.error(&e);
                            Cmd::Continue
                        }
                    },
                    None => {
                        // A skill is read at startup, so one written this
                        // session is not in the list yet: say so where it is hit.
                        console.error(&format!(
                            "unknown skill: {name} (/skills lists what loaded; a new skill needs a restart)"
                        ));
                        Cmd::Continue
                    }
                }
            }
            Command::Unknown(input) => {
                console.error(&format!("unknown command: {input}"));
                Cmd::Continue
            }
            Command::Shell(command) => Cmd::Shell(command),
            Command::Turn(input) => Cmd::Turn(input),
        }
    }

    /// `/sessions`: the chooser when there is a screen, a numbered list when
    /// there is not.
    fn sessions(&mut self, query: &str, console: &mut Console) {
        match console {
            Console::Screen { screen, rx } => self.browse_sessions(screen, rx, query),
            Console::Stdout => self.list_piped(query),
        }
    }
}

// ---- the live prompt ----

/// What the live prompt owns while a turn runs: the lines waiting for a
/// boundary, the interrupt's two-press arming, and the text that is not a
/// finished line yet. The turn's thread owns the history; this owns what a key
/// or an event does to the frame.
#[derive(Default)]
struct Held {
    /// Submitted lines, oldest first.
    queued: Vec<Pending>,
    /// Lines Up took back after the turn had read them: their echo is still
    /// owed, and it must not take a waiting line off the frame with it.
    awaiting: usize,
    /// Streamed text up to the last newline is permanent; the rest is the line
    /// still being written.
    partial: String,
    /// Reasoning before the answer. Its last line is unfinished until the
    /// answer starts or the round ends, so it is held until then.
    thought: String,
    /// A mode switch pressed while the turn runs. Shift+Tab reached the live
    /// prompt; it is applied at the boundary, because the turn's tool set and
    /// history are fixed before it starts.
    switch: bool,
    /// ESC asks first: one press offers the interrupt, the next takes it.
    armed: bool,
    /// Whether the turn was actually stopped, for the line that says so.
    interrupted: bool,
}

impl Held {
    /// One event from a producer. Returns whether the turn is over.
    fn event(
        &mut self,
        event: Event,
        screen: &mut ui::Screen,
        queue: &Queue,
        cancel: &http::Cancel,
        used: &mut Option<u64>,
    ) -> bool {
        match event {
            Event::Key(key) => return self.key(key, screen, queue, cancel),
            Event::Paste(text) => screen.insert_text(&text),
            // The turn took a queued line into its history: it is no longer
            // pending, and it belongs in the transcript at the point the model
            // saw it. The queue is read oldest first, which is the row the
            // frame drops.
            Event::User(line) => self.took(&line, screen),
            // Model text, already spaced by the sink.
            Event::Printed(text) => self.printed(&text, screen),
            // A tool call and its outcome, on one line.
            Event::Echo(spans, error) => {
                // A round that only reasoned and called a tool: its thought
                // ends here, before the call it produced.
                let tail = std::mem::take(&mut self.thought);
                screen.thought(&tail);
                // The command that was running is in the transcript now; the
                // status row goes back to being empty.
                screen.set_status("");
                screen.insert(vec![line_of(&spans)], ui::Block::Tool);
                if let Some(error) = &error {
                    screen.fail(error);
                }
            }
            // Reasoning, dim: it precedes the answer and is not the answer, and
            // it must not disturb the status row.
            Event::Thought(text) => self.reasoned(&text, screen),
            Event::Status(text) => {
                if screen.status() != text {
                    screen.set_status(&text);
                }
            }
            // The round is not a whole answer: the line says what cut it, and
            // the status row belongs to the next round.
            Event::Cut(why) => {
                screen.set_status("");
                screen.error(&format!("axe: {why}\n"));
            }
            // The area under us changed: paint even if no state did.
            Event::Resize => screen.invalidate(),
            Event::Done(Err(e)) => {
                screen.error(&format!("axe: {e}\n"));
                return true;
            }
            Event::Done(Ok(count)) => {
                *used = count;
                let tail = std::mem::take(&mut self.thought);
                screen.thought(&tail);
                // The last block has no ending line of its own.
                screen.flush();
                self.partial.clear();
                return true;
            }
            Event::Eof => {
                // Nothing more can arrive: the stream stops here, and a cut
                // reply must not read like a finished one.
                self.interrupted = true;
                cancel.cancel();
                return true;
            }
        }
        false
    }

    /// A key reached the live prompt. ESC is the interrupt and takes two
    /// presses: the first offers it and any other key withdraws the offer, so
    /// a mistyped ESC cannot end a turn that was nearly done. The flag ends the
    /// turn between requests; the socket shutdown unblocks the one in flight.
    fn key(
        &mut self,
        key: KeyEvent,
        screen: &mut ui::Screen,
        queue: &Queue,
        cancel: &http::Cancel,
    ) -> bool {
        if key.code == KeyCode::Esc {
            if self.armed {
                self.interrupted = true;
                cancel.cancel();
                screen.set_alert(Some("× Interrupting"));
            } else {
                self.armed = true;
                screen.set_alert(Some("× press ESC again to interrupt"));
            }
            return false;
        }
        if self.armed {
            self.armed = false;
            screen.set_alert(None);
        }
        match screen.apply_key(key) {
            ui::Action::Eof => {
                // End of input stops the turn it meets, and says so: the reply
                // it cuts is partial.
                self.interrupted = true;
                cancel.cancel();
                return true;
            }
            ui::Action::Submit => self.submit(screen, queue),
            // Up: the waiting lines come back to the prompt, above what was
            // typed, and the frame lets go of them.
            ui::Action::Recall => self.recall(screen, queue),
            // The turn already captured its tool set and its history prefix;
            // the switch is applied at the boundary, after the answer it did
            // not affect.
            ui::Action::Explore => self.switch = true,
            // A chooser only exists while idle.
            ui::Action::MenuStep(_) | ui::Action::MenuAccept | ui::Action::MenuCancel => {}
            ui::Action::None => {}
        }
        false
    }

    /// A submitted line. Queued without echoing here: the transcript shows the
    /// line where the model saw it, which is the round boundary the turn takes
    /// it at, or the turn's end for a command. A plain request reaches the
    /// model between rounds; a command needs the session and the screen, so it
    /// waits for the REPL loop.
    fn submit(&mut self, screen: &mut ui::Screen, queue: &Queue) {
        if screen.is_empty() {
            return;
        }
        let line = screen.take();
        let turn = match parse_command(&line) {
            Command::Turn(input) => {
                if let Ok(mut lines) = queue.lock() {
                    lines.push_back(input);
                }
                true
            }
            _ => false,
        };
        self.queued.push(Pending { line, turn });
        show_queue(screen, &self.queued);
    }

    fn recall(&mut self, screen: &mut ui::Screen, queue: &Queue) {
        let (kept, read) = take_back(queue, &self.queued);
        self.awaiting += read;
        self.queued.clear();
        show_queue(screen, &self.queued);
        recall_lines(screen, &kept);
    }

    /// The turn read the oldest line still on the frame. One Up took back was
    /// read too, so only its echo is due.
    fn took(&mut self, line: &str, screen: &mut ui::Screen) {
        if self.awaiting > 0 {
            self.awaiting -= 1;
        } else if let Some(at) = self.queued.iter().position(|entry| entry.turn) {
            self.queued.remove(at);
            show_queue(screen, &self.queued);
        }
        screen.echo_user(line);
    }

    /// Model text: everything up to the last newline is a finished line.
    fn printed(&mut self, text: &str, screen: &mut ui::Screen) {
        // The answer starting is the end of the thought: its unfinished last
        // line goes out first.
        let tail = std::mem::take(&mut self.thought);
        screen.thought(&tail);
        self.partial.push_str(text);
        // Lines end at a newline; blank lines at the tail are the separator
        // before whatever comes next, so they wait. A chunk boundary cannot
        // then split one paragraph break into two.
        let tail = self.partial.len() - self.partial.trim_end_matches('\n').len();
        if tail > 0 {
            let end = self.partial.len() - (tail - 1);
            let finished: String = self.partial.drain(..end).collect();
            if !finished.is_empty() {
                screen.text(&finished);
            }
        }
        // The unfinished line is not the status: the row says what the turn is
        // doing, not what it says.
        screen.set_status("▸ Responding");
    }

    fn reasoned(&mut self, text: &str, screen: &mut ui::Screen) {
        self.thought.push_str(text);
        let finished = match self.thought.rfind('\n') {
            Some(at) => self.thought.drain(..=at).collect::<String>(),
            None => String::new(),
        };
        screen.thought(&finished);
    }
}

// ---- output ----

/// Where a command's output goes. The screen writes it into the transcript;
/// without a terminal, stdout gets the same text. Only a chooser needs the
/// screen itself, so the console hands it out and the other arms never ask.
enum Console<'a> {
    Screen {
        screen: &'a mut ui::Screen,
        rx: &'a Receiver<Event>,
    },
    Stdout,
}

impl Console<'_> {
    fn output(&mut self, text: &str) {
        match self {
            Console::Screen { screen, .. } => screen.output(text),
            Console::Stdout => {
                print!("{}", terminal_text(text));
                if !text.ends_with('\n') {
                    println!();
                }
            }
        }
    }

    /// The model's words when no turn is around them: `/compact`'s summary is
    /// markdown like an answer, written as its own block; a pipe renders it the
    /// same way it renders a turn's text.
    fn text(&mut self, text: &str) {
        match self {
            Console::Screen { screen, .. } => {
                screen.text(text);
                screen.flush();
            }
            Console::Stdout => {
                for line in markdown::document(text, 0) {
                    println!("{}", markdown::ansi(&line));
                }
            }
        }
    }

    /// A failure, with the `axe:` the user reads and the model never sees.
    fn error(&mut self, text: &str) {
        let text = strip_control(text);
        match self {
            Console::Screen { screen, .. } => screen.error(&format!("axe: {text}\n")),
            Console::Stdout => eprintln!("axe: {text}"),
        }
    }

    fn screen(&mut self) -> Option<&mut ui::Screen> {
        match self {
            Console::Screen { screen, .. } => Some(screen),
            Console::Stdout => None,
        }
    }

    /// The selection row: a screen has one, a pipe does not.
    fn info(&mut self, parts: Vec<(ui::Info, String)>) {
        if let Some(screen) = self.screen() {
            screen.set_info(parts);
        }
    }

    /// The prompt's box, which changes color with the tool set.
    fn explore(&mut self, on: bool) {
        if let Some(screen) = self.screen() {
            screen.set_explore(on);
        }
    }
}

// ---- the sinks ----

struct ChannelSink {
    tx: Sender<Event>,
    out: TextSink,
    /// Reasoning is shown in the transcript unless `/thoughts` turned it off.
    thoughts: bool,
    /// When the hidden reasoning of this round began, for the one line that
    /// replaces it in the transcript.
    thought: Option<Instant>,
}

impl ChannelSink {
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Hidden reasoning ends at the first thing that follows it: the answer,
    /// a tool call, or the round itself. What reaches the transcript then is
    /// its duration, on one line, where the thought would have been.
    fn end_thought(&mut self) {
        if let Some(start) = self.thought.take() {
            self.send(Event::Thought(format!(
                "Thought ({:.1}s)\n",
                start.elapsed().as_secs_f32()
            )));
        }
    }
}

impl Sink for ChannelSink {
    fn text(&mut self, text: &str) {
        // Some providers send `content: ""` on every delta, reasoning ones
        // included. Empty text is not the answer starting, so it must not end
        // the thought: that would put one "Thought (0.0s)" line per chunk.
        if text.is_empty() {
            return;
        }
        self.end_thought();
        let mut done = String::new();
        self.out.push(text, &mut done);
        if !done.is_empty() {
            self.send(Event::Printed(done));
        }
    }

    fn status(&mut self, text: &str) {
        self.send(Event::Status(text.to_string()));
    }

    fn echo(&mut self, call: &[(Paint, String)], mark: (Paint, &'static str), error: Option<&str>) {
        self.end_thought();
        let mut spans = call.to_vec();
        spans.push((Paint::Plain, " ".to_string()));
        spans.push((mark.0, mark.1.to_string()));
        self.send(Event::Echo(spans, error.map(str::to_string)));
    }

    fn cut(&mut self, why: &str) {
        self.end_thought();
        self.send(Event::Cut(why.to_string()));
    }

    fn end_round(&mut self) {
        self.end_thought();
        self.out.commit();
        if self.out.take_wrote() {
            // The reply ended without a final newline, so the line it left
            // unfinished becomes permanent here.
            self.send(Event::Printed("\n".to_string()));
        }
    }

    fn user(&mut self, line: &str) {
        self.end_thought();
        // The block above is finished: the blank line between the two is the
        // writer's, not this sink's.
        self.out.commit();
        self.send(Event::User(line.to_string()));
    }

    fn reasoning(&mut self, text: &str) {
        // The model working, shown or not: the row says so either way. The
        // turn loop drops a repeat, so one per delta costs nothing extra.
        self.send(Event::Status("⋯ Thinking".to_string()));
        if self.thoughts {
            self.send(Event::Thought(text.to_string()));
        } else {
            // The thought is hidden, but not its cost: its first chunk starts
            // the clock, and `end_thought` puts the duration in its place.
            self.thought.get_or_insert_with(Instant::now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::crossterm::event::KeyModifiers;

    use crate::text::oneline;

    /// The info row as one line: the screen joins the pieces, and so does
    /// anything that asserts on them.
    fn info_text(session: &Session) -> String {
        ui::info_text(&session.info())
    }

    #[test]
    fn control_bytes_are_stripped_but_lines_survive() {
        assert_eq!(strip_control("a\x1b[31mb\nc\td\u{9b}"), "a[31mb\nc\td");
    }

    #[test]
    fn parses_commands_once() {
        assert_eq!(parse_command("  "), Command::Empty);
        assert_eq!(parse_command("/model"), Command::Model(None, Scope::Dir));
        assert_eq!(
            parse_command("/model a/b"),
            Command::Model(Some("a/b".into()), Scope::Dir)
        );
        // `-g`/`--global` is a flag as a whole word; a spec that merely starts
        // with it is still a spec, and a search still narrows the chooser.
        assert_eq!(
            parse_command("/model -g"),
            Command::Model(None, Scope::Global)
        );
        assert_eq!(
            parse_command("/model --global a/b"),
            Command::Model(Some("a/b".into()), Scope::Global)
        );
        assert_eq!(
            parse_command("/model -g  open/code"),
            Command::Model(Some("open/code".into()), Scope::Global)
        );
        assert_eq!(
            parse_command("/model -gpt"),
            Command::Model(Some("-gpt".into()), Scope::Dir)
        );
        assert_eq!(parse_command("/thinking"), Command::Thinking(None));
        assert_eq!(
            parse_command("/thinking high"),
            Command::Thinking(Some("high".into()))
        );
        assert_eq!(
            parse_command("/sessions q"),
            Command::Sessions(Some("q".into()))
        );
        assert_eq!(parse_command("/compact"), Command::Compact);
        assert!(COMMANDS.contains(&"/compact"));
        assert_eq!(parse_command("! ls"), Command::Shell("ls".into()));
        assert_eq!(parse_command("!"), Command::Empty);
        assert_eq!(parse_command("hello"), Command::Turn("hello".into()));
        assert_eq!(parse_command("/skills"), Command::Skills);
        assert_eq!(
            parse_command("/skill:pdf-tools extract it"),
            Command::Skill("pdf-tools".into(), "extract it".into())
        );
        assert_eq!(
            parse_command("/skill:pdf-tools"),
            Command::Skill("pdf-tools".into(), String::new())
        );
        assert_eq!(parse_command("/skill:"), Command::Unknown("/skill:".into()));
        assert_eq!(parse_command("/modelx"), Command::Unknown("/modelx".into()));
        // The listing lives on `/model` now; the old spelling is not an alias.
        assert_eq!(parse_command("/models"), Command::Unknown("/models".into()));
    }

    #[test]
    fn naming_parses_two_lines() {
        assert_eq!(
            parse_naming("TITLE: Fix parser\nSUMMARY: The parser was fixed."),
            Some(("Fix parser".into(), "The parser was fixed.".into()))
        );
        assert_eq!(parse_naming("nothing useful"), None);
    }

    #[test]
    fn compaction_prompt_keeps_the_newest_and_counts_what_it_dropped() {
        let messages = vec![
            json!({ "role": "system", "content": "the prompt" }),
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "first" }),
            json!({ "role": "assistant", "content": "second" }),
            json!({ "role": "user", "content": "third" }),
        ];
        // Wide enough: everything the session said, and neither the system
        // prompt nor the mode note.
        let all = compaction_prompt(&messages, 10_000);
        assert!(all.contains("user: first"));
        assert!(all.contains("assistant: second"));
        assert!(all.contains("user: third"));
        assert!(!all.contains("the prompt"));
        assert!(!all.contains("explore mode"));
        // Tight enough for one message: the oldest goes first, the newest stays.
        let one = compaction_prompt(&messages, 10);
        assert!(one.contains("third"));
        assert!(!one.contains("first"));
        assert!(one.starts_with("["));
        assert!(one.contains("earlier messages omitted"));
        // Nothing to say is not a panic.
        assert_eq!(compaction_prompt(&messages[..2], 10), "");
    }

    #[test]
    fn a_tool_result_is_cut_before_it_is_summarized() {
        let long = "x".repeat(5000);
        let messages = vec![
            json!({ "role": "system", "content": "s" }),
            json!({ "role": "user", "content": "u" }),
            json!({ "role": "tool", "content": long }),
            json!({ "role": "user", "content": "newest" }),
        ];
        let prompt = compaction_prompt(&messages, 10_000);
        assert!(prompt.contains("…[cut]…"));
        assert!(prompt.len() < 1500);
        assert!(prompt.contains("newest"));
        // Short text is left alone.
        assert_eq!(head_tail("abc", 2, 2), "abc");
    }

    #[test]
    fn help_lists_every_command_and_the_config() {
        for name in COMMANDS {
            assert!(HELP.contains(name), "{name} is missing from HELP");
        }
        let text = help_text();
        assert!(text.contains(&*config::path().to_string_lossy()));
        assert!(text.contains("explore_by_default"));
    }

    #[test]
    fn the_cli_answers_before_it_runs() {
        let args = |line: &str| {
            line.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(cli(args("").into_iter()), Cli::Run);
        for flag in ["--version", "-V"] {
            assert_eq!(cli(args(flag).into_iter()), Cli::Version, "{flag}");
        }
        for flag in ["--help", "-h"] {
            assert_eq!(cli(args(flag).into_iter()), Cli::Help, "{flag}");
        }
        // Anything else is named, not ignored: a flag that does nothing reads
        // as a setting that did nothing.
        for arg in ["--nope", "notes.txt"] {
            assert_eq!(
                cli(args(arg).into_iter()),
                Cli::Unknown(arg.to_string()),
                "{arg}"
            );
        }
        let text = usage();
        for needle in ["--version", "--help", "< prompts", "commands:"] {
            assert!(text.contains(needle), "{needle} missing from usage");
        }
        assert!(text.contains(&*config::path().to_string_lossy()));
    }

    #[test]
    fn help_names_the_keys_and_a_new_directory_is_told_where_to_start() {
        // The prompt is the only place a user learns these without the README.
        for needle in ["Tab", "Ctrl+R", "Shift+Tab", "keys"] {
            assert!(HELP.contains(needle), "{needle} missing from HELP");
        }
        let explore = startup::first_run_note(tools::Mode::Explore);
        assert!(explore.contains("/explore"), "{explore}");
        for note in [explore, startup::first_run_note(tools::Mode::Normal)] {
            assert!(note.contains("/help") && note.contains("Tab"), "{note}");
        }
        // Normal mode has no write tool to explain, so the note must not name
        // the toggle that would turn one on.
        assert!(!startup::first_run_note(tools::Mode::Normal).contains("/explore"));
    }

    #[test]
    fn display_helpers() {
        assert_eq!(scaled(9999), "9999");
        assert_eq!(scaled(10_000), "10k");
        assert_eq!(scaled(1_000_000), "1.0M");
        assert_eq!(percent(1, 1000), "<1%");
        assert_eq!(percent(50, 100), "50%");
        // A provider's absurd count is a number to print, not an overflow.
        assert_eq!(percent(u64::MAX, 1), "1844674407370955161500%");
        assert_eq!(fullness(u64::MAX, 1), 100);
        assert!(failed("bash", "exit: 1\nboom"));
        assert!(!failed("bash", "exit: 0\nok"));
        assert!(failed("tools", "error: nope"));
        assert_eq!(mark("bash", "exit: 0\n").1, "✓");
        assert_eq!(mark("bash", "exit: 1\n").1, "✗");
        assert_eq!(detail("exit: 1\nboom\nmore"), "boom…");
        assert_eq!(oneline("a\nb", 10), "a…");
    }

    #[test]
    fn session_ids_are_32_hex_chars() {
        let id = startup::session_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn naming_excerpt_skips_mode_and_shell_lines() {
        let messages = vec![
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "$ !ls\nfile" }),
            json!({ "role": "user", "content": "fix it" }),
            json!({ "role": "assistant", "content": "fixed" }),
        ];
        assert_eq!(
            naming_excerpt(&messages),
            Some(("fix it".into(), "fixed".into()))
        );
    }

    #[test]
    fn model_selection_resolves_provider_and_bare_id() {
        let provider = Provider {
            name: "p".into(),
            cfg: config::ProviderConfig {
                name: "p".into(),
                base_url: "http://p".into(),
                api_key_env: None,
                models_url: None,
                session_header: None,
                thinking: Vec::new(),
            },
            ep: http::parse_url("http://p").unwrap(),
            key: None,
            headers: Vec::new(),
            models: vec![api::ModelInfo {
                id: "m".into(),
                context: Some(100),
            }],
        };
        assert_eq!(
            startup::find(std::slice::from_ref(&provider), "p/m"),
            Some((0, "m".into()))
        );
        assert_eq!(
            startup::find(std::slice::from_ref(&provider), "m"),
            Some((0, "m".into()))
        );
        assert_eq!(
            startup::resolve(std::slice::from_ref(&provider), [Some("p/m"), None, None]),
            (0, "m".into())
        );
        assert_eq!(
            startup::resolve(
                std::slice::from_ref(&provider),
                [Some("missing"), Some("p/m"), None]
            ),
            (0, "m".into())
        );
    }

    fn test_provider() -> Provider {
        Provider {
            name: "p".into(),
            cfg: config::ProviderConfig {
                name: "p".into(),
                base_url: "http://p".into(),
                api_key_env: None,
                models_url: None,
                session_header: None,
                thinking: Vec::new(),
            },
            ep: http::parse_url("http://p").unwrap(),
            key: None,
            headers: Vec::new(),
            models: vec![api::ModelInfo {
                id: "m".into(),
                context: Some(100),
            }],
        }
    }

    #[test]
    fn session_switches_model_and_new_session() {
        let mut session = Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            mode: tools::Mode::Explore,
            used: Some(10),
            messages: vec![json!({ "role": "system", "content": "s" })],
            bytes: 40,
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: vec!["old".into()],
            naming: None,
            warning: None,
            skills: Vec::new(),
        };
        let info = ui::info_text(&session.set_model(0, "m".into(), Scope::Dir));
        assert!(info.contains("p/m"), "{info}");
        assert_eq!(session.used, None);
        assert_eq!(session.state.model.as_deref(), Some("p/m"));
        session.new_session(None);
        assert!(session.picks.is_empty());
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0]["content"].as_str(), Some("s"));
    }

    #[test]
    fn a_global_choice_becomes_the_default_and_drops_the_directorys_pin() {
        state::testenv::home("model-default", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let mut session = Session {
                state: state::load(&cwd),
                providers: vec![test_provider()],
                provider: 0,
                model: "m".into(),
                thinking: None,
                mode: tools::Mode::Explore,
                used: None,
                messages: Vec::new(),
                bytes: 0,
                sid: "abc".into(),
                store: None,
                startup: Vec::new(),
                picks: Vec::new(),
                naming: None,
                warning: None,
                skills: Vec::new(),
            };
            session.set_model(0, "m".into(), Scope::Dir);
            assert_eq!(state::load(&cwd).model.as_deref(), Some("p/m"));

            // The default a directory with no pin starts on; this directory's
            // pin would shadow it, so it goes.
            session.set_model(0, "m".into(), Scope::Global);
            assert_eq!(state::load_default().as_deref(), Some("p/m"));
            assert_eq!(state::load(&cwd).model, None);
            assert!(session.warning.is_none());
        });
    }

    #[test]
    fn summaries_and_session_rows() {
        let meta = sessions::Meta {
            id: "abc123".into(),
            created: 0,
            updated: 0,
            title: "Title".into(),
            summary: String::new(),
            messages: 3,
            archived: true,
        };
        assert_eq!(session_title(&meta), "Title");
        assert!(session_row(&meta).contains("Title"));
        assert!(session_row(&meta).contains("zip"));
    }

    #[test]
    fn call_and_shell_lines_are_compact() {
        let read = call_line(
            &api::ToolCall {
                id: "1".into(),
                name: "read".into(),
                arguments: "{\"path\":\"src/ui.rs\",\"offset\":2,\"limit\":3}".into(),
            },
            true,
        );
        let text = plain_text(&read);
        assert!(text.contains("read"));
        assert!(text.contains("src/ui.rs"));
        assert!(text.contains("2+3"));
        let edit = call_line(
            &api::ToolCall {
                id: "2".into(),
                name: "edit".into(),
                arguments: "{\"path\":\"src/ui.rs\",\"old\":\"a\\nb\",\"new\":\"c\"}".into(),
            },
            false,
        );
        let text = plain_text(&edit);
        assert!(text.contains("src/ui.rs"));
        assert!(text.contains("2→1 lines"));
        // A font that lacks the glyph draws it wide: the second space is the
        // cell its ink covers, so the name keeps its gap.
        let about = call_line(
            &api::ToolCall {
                id: "3".into(),
                name: "about".into(),
                arguments: "{}".into(),
            },
            false,
        );
        assert_eq!(plain_text(&about), "ℹ  about");
        let shell = shell_line("echo hi", false);
        assert!(plain_text(&shell).contains("echo hi"));
    }

    #[test]
    fn channel_sink_emits_text_and_hidden_thoughts() {
        let (tx, rx) = mpsc::channel();
        let mut sink = ChannelSink {
            tx,
            out: TextSink::default(),
            thoughts: false,
            thought: None,
        };
        sink.reasoning("think");
        sink.text("hi");
        sink.end_round();
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(events.iter().any(|event| matches!(event, Event::Status(_))));
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Printed(_))));
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::Thought(_))));
    }

    #[test]
    fn skills_appear_in_listing_commands_and_new_history() {
        let skill = skills::Skill {
            name: "demo".into(),
            description: "demo skill".into(),
            file: std::path::PathBuf::from("/repo/.agents/skills/demo/SKILL.md"),
            scope: skills::Scope::Project,
            allowed_tools: vec!["Read".into()],
            hidden: false,
        };
        let session = Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            mode: tools::Mode::Explore,
            used: None,
            messages: vec![json!({ "role": "system", "content": "s" })],
            bytes: 40,
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: Vec::new(),
            naming: None,
            warning: None,
            skills: vec![skill],
        };
        assert!(session.commands().contains(&"/skill:demo".to_string()));
        let listing = session.skill_listing();
        assert!(listing.contains("demo"));
        assert!(listing.contains("tools: Read"));
        let base = session.base_messages();
        assert_eq!(base.len(), 2);
        assert_eq!(base[0]["content"].as_str(), Some("s"));
    }

    /// A session with a system prompt and nothing else, for the fields that do
    /// not need a terminal or a directory.
    fn bare_session() -> Session {
        let messages = vec![json!({ "role": "system", "content": "s" })];
        Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            mode: tools::Mode::Explore,
            used: None,
            bytes: history_bytes(&messages),
            messages,
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: Vec::new(),
            naming: None,
            warning: None,
            skills: Vec::new(),
        }
    }

    #[test]
    fn the_history_size_is_counted_as_the_history_grows() {
        let mut session = bare_session();
        session.record(json!({ "role": "user", "content": "hi" }));
        session.set_mode(tools::Mode::Normal);
        assert_eq!(session.bytes, history_bytes(&session.messages));
        // The estimate the info row falls back to is that sum, four to a token.
        assert!(
            session.fill().0.starts_with(&scaled(session.bytes / 4)),
            "{} of {}",
            session.fill().0,
            session.bytes
        );
        // A replacement re-counts rather than carrying the old sum.
        let grown = session.bytes;
        session.messages = session.base_messages();
        session.bytes = history_bytes(&session.messages);
        assert!(session.bytes < grown);
        assert_eq!(session.bytes, history_bytes(&session.messages));
    }

    #[test]
    fn a_weaker_sandbox_joins_the_history() {
        let system = json!({ "role": "system", "content": "s" });
        let held = base_history(system.clone(), tools::Mode::Explore, None);
        assert_eq!(held.len(), 2, "the prompt and the mode note");
        let weak = base_history(
            system,
            tools::Mode::Explore,
            Some("sandbox: Landlock is unavailable"),
        );
        assert_eq!(weak.len(), 3);
        assert_eq!(weak[2]["role"], "user");
        assert_eq!(weak[2]["content"], "sandbox: Landlock is unavailable");
    }

    #[test]
    fn session_info_and_mode_note() {
        let mut session = Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            mode: tools::Mode::Explore,
            used: None,
            messages: vec![json!({ "role": "system", "content": "s" })],
            bytes: 40,
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: Vec::new(),
            naming: None,
            warning: None,
            skills: Vec::new(),
        };
        assert!(info_text(&session).contains("p/m"));
        assert_eq!(session.context(), Some(100));
        assert!(session.fill().0.contains("100"));
        session.set_mode(tools::Mode::Normal);
        assert!(!session.mode.explore());
        assert_eq!(session.messages.len(), 2);
        session.record(json!({ "role": "user", "content": "hi" }));
        assert_eq!(session.messages.len(), 3);
        session.warn("careful".into());
        session.flush_warnings(None);
    }

    #[test]
    fn session_info_carries_the_title_left_of_the_model() {
        let meta = sessions::Meta::new("abc");
        let mut session = Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            mode: tools::Mode::Normal,
            used: None,
            messages: Vec::new(),
            bytes: 0,
            sid: "abc".into(),
            store: {
                let mut store = Store::fresh(std::env::temp_dir(), "abc");
                store.meta = Some(meta);
                Some(store)
            },
            startup: Vec::new(),
            picks: Vec::new(),
            naming: None,
            warning: None,
            skills: Vec::new(),
        };
        // Untitled and empty history: the row starts with the model.
        assert!(
            info_text(&session).starts_with("p/m"),
            "{}",
            info_text(&session)
        );
        // The first request stands in until the model names the session; a
        // mode note or a shell line is not a request.
        session.messages = vec![
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "$ !ls\nfile" }),
            json!({ "role": "user", "content": "why is the row blank" }),
        ];
        assert!(
            info_text(&session).starts_with("why is the row blank  ·  p/m"),
            "{}",
            info_text(&session)
        );
        // A generated title wins over the prompt.
        session.store.as_mut().unwrap().meta.as_mut().unwrap().title = "Fix the status row".into();
        assert!(info_text(&session).starts_with("Fix the status row  ·  p/m"));
        // A title of spaces is no title: the prompt is back.
        session.store.as_mut().unwrap().meta.as_mut().unwrap().title = "  ".into();
        assert!(
            info_text(&session).starts_with("why is the row blank"),
            "{}",
            info_text(&session)
        );
    }

    #[test]
    fn the_info_row_names_the_directory_left_of_the_model() {
        // `dir_label` reads `HOME` itself, so the test holds the environment
        // still: a scratch home another test writes between the two reads is
        // a path neither of them meant.
        state::testenv::home("info-row-dir", |home| {
            let mut session = bare_session();
            session.state.cwd = home.join("work");
            let text = info_text(&session);
            assert!(text.starts_with("~/work  ·  p/m"), "{text}");
            // The home directory itself is the one path with nothing after the
            // `~`, and a path outside it keeps its own name.
            session.state.cwd = home.to_path_buf();
            let text = info_text(&session);
            assert!(text.starts_with("~  ·  p/m"), "{text}");
            session.state.cwd = std::path::PathBuf::from("/tmp/x");
            let text = info_text(&session);
            assert!(text.starts_with("/tmp/x  ·  p/m"), "{text}");
            // A session with no working directory has nothing to say: the row
            // starts at the model, with no empty piece and no stray separator.
            session.state.cwd = std::path::PathBuf::new();
            let text = info_text(&session);
            assert!(text.starts_with("p/m"), "{text}");
            // A sibling whose name starts with the home path is not under it.
            let sibling = std::path::PathBuf::from(format!("{}-old", home.display()));
            assert_eq!(dir_label(&sibling), sibling.display().to_string());
        });
    }

    use crate::http::support::{reply, sse, Server};

    /// A session pointed at a loopback provider, with the two history
    /// messages every session starts with.
    fn session_at(server: &Server) -> Session {
        let mut session = bare_session();
        session.messages = session.base_messages();
        session.bytes = history_bytes(&session.messages);
        let provider = &mut session.providers[0];
        provider.ep = server.ep.clone();
        provider.models = vec![api::ModelInfo {
            id: "m".into(),
            context: Some(1000),
        }];
        session
    }

    /// The pair a turn's events travel on.
    fn events() -> (Sender<Event>, Receiver<Event>) {
        mpsc::channel()
    }

    #[test]
    fn a_turn_streams_into_the_screen_and_the_history() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{"content":"\n"}}]}"#,
            r#"{"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
            "[DONE]",
        ])]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(40, 20);

        let leftover = session
            .messages_turn("say hello", &tx, &rx, &mut screen)
            .unwrap();
        assert!(leftover.is_empty());
        assert_eq!(session.messages[2]["content"], "say hello");
        assert_eq!(session.messages[3]["role"], "assistant");
        assert_eq!(session.messages[3]["content"], "hello\n");
        assert_eq!(session.used, Some(4), "the round counted its tokens");
        assert!(screen.status().is_empty(), "the status row is cleared");
        let rows = screen.transcript();
        assert!(rows.iter().any(|row| row.contains("hello")), "{rows:?}");
    }

    #[test]
    fn a_turn_that_calls_a_tool_shows_the_call_it_ran() {
        let server = Server::new(vec![
            sse(&[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"about","arguments":"{}"}}]}}]}"#,
                "[DONE]",
            ]),
            sse(&[r#"{"choices":[{"delta":{"content":"done"}}]}"#, "[DONE]"]),
        ]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);

        session
            .messages_turn("look it up", &tx, &rx, &mut screen)
            .unwrap();
        assert_eq!(session.messages[2 + 1]["tool_calls"][0]["id"], "c1");
        assert_eq!(session.messages[2 + 2]["role"], "tool");
        let rows = screen.transcript();
        assert!(rows.iter().any(|row| row.contains("about")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("done")), "{rows:?}");
    }

    #[test]
    fn a_refused_turn_is_reported_on_the_screen() {
        let server = Server::new(vec![reply("500 Server Error", &[], "it broke")]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);

        let leftover = session
            .messages_turn("do it", &tx, &rx, &mut screen)
            .unwrap();
        assert!(leftover.is_empty());
        assert_eq!(session.messages.len(), 3, "only the request was recorded");
        let rows = screen.transcript();
        assert!(rows.iter().any(|row| row.contains("it broke")), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.starts_with(" axe:")),
            "the failure is the binary's line, not the model's: {rows:?}"
        );
    }

    #[test]
    fn esc_asks_first_and_the_second_press_stops_the_turn() {
        let server = Server::new(vec![sse(&["[DONE]"])]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);
        // Both presses are already waiting when the turn starts: the first
        // offers the interrupt, the second takes it.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        tx.send(Event::Key(esc)).unwrap();
        tx.send(Event::Key(esc)).unwrap();

        session
            .messages_turn("stop me", &tx, &rx, &mut screen)
            .unwrap();
        let rows = screen.transcript();
        assert!(
            rows.iter().any(|row| row.contains("Turn interrupted")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_cut_reply_says_so_in_the_transcript() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"half"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            "[DONE]",
        ])]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);
        session.messages_turn("go", &tx, &rx, &mut screen).unwrap();
        let rows = screen.transcript();
        assert!(rows.iter().any(|row| row.contains("half")), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.contains("length limit")),
            "the round is not a whole answer and says so: {rows:?}"
        );
        // The text before the cut is the model's, so the session keeps it.
        assert_eq!(session.messages.last().unwrap()["content"], "half");
    }

    #[test]
    fn a_paste_and_a_resize_reach_the_screen() {
        let server = Server::new(vec![sse(&["[DONE]"])]);
        let mut session = session_at(&server);
        let (tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);
        tx.send(Event::Paste("pasted text".into())).unwrap();
        tx.send(Event::Resize).unwrap();
        tx.send(Event::Eof).unwrap();

        session.messages_turn("go", &tx, &rx, &mut screen).unwrap();
        // The paste went into the line, which the turn never took: the event
        // loop is the only place it could land.
        assert_eq!(screen.take(), "pasted text");
    }

    #[test]
    fn every_command_is_dispatched_by_name() {
        let mut session = bare_session();
        let mut console = Console::Stdout;
        assert!(matches!(session.dispatch("/exit", &mut console), Cmd::Exit));
        assert!(matches!(session.dispatch("", &mut console), Cmd::Continue));
        assert!(matches!(
            session.dispatch("/help", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/new", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/model", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/model p/m", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/model nope", &mut console),
            Cmd::Continue
        ));
        assert_eq!(session.model, "m", "an unknown spec changes nothing");
        assert!(matches!(
            session.dispatch("/thinking", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/thinking off", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/thinking nope", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/thoughts", &mut console),
            Cmd::Continue
        ));
        assert!(session.state.thoughts);
        assert!(matches!(
            session.dispatch("/explore", &mut console),
            Cmd::Continue
        ));
        assert_eq!(session.mode, tools::Mode::Normal);
        assert!(matches!(
            session.dispatch("/sessions", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/compact", &mut console),
            Cmd::Compact
        ));
        assert!(matches!(
            session.dispatch("/skills", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/skill:none go", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("/bogus", &mut console),
            Cmd::Continue
        ));
        assert!(matches!(
            session.dispatch("!echo hi", &mut console),
            Cmd::Shell(command) if command == "echo hi"
        ));
        assert!(matches!(
            session.dispatch("a request", &mut console),
            Cmd::Turn(request) if request == "a request"
        ));
    }

    #[test]
    fn a_label_is_capital_and_its_value_is_not() {
        // A command leaves one line naming the setting, capital, then its
        // state: lowercase would read as prose, which a notice is not.
        state::testenv::home("label-capitals", |_home| {
            let mut session = bare_session();
            let mut screen = ui::Screen::test_screen(80, 12);
            let (_tx, rx) = std::sync::mpsc::channel();
            {
                let mut console = Console::Screen {
                    screen: &mut screen,
                    rx: &rx,
                };
                session.dispatch("/explore", &mut console);
                session.dispatch("/thoughts", &mut console);
                // Both `/thinking` arms: the bare line and a level that
                // `choose_level` applies.
                session.dispatch("/thinking", &mut console);
                session.dispatch("/thinking off", &mut console);
                session.dispatch("/model -g m", &mut console);
            }
            let rows = screen.transcript();
            for line in [
                "Explore: off",
                "Thoughts: on",
                "Thinking: default (not set)",
                "Thinking: off",
                "◦ Default model: p/m",
            ] {
                assert!(
                    rows.iter().any(|row| row.contains(line)),
                    "{line}: {rows:?}"
                );
            }
        });
    }

    #[test]
    fn a_new_session_begins_read_only_again() {
        let mut session = bare_session();
        let mut console = Console::Stdout;
        session.set_mode(tools::Mode::Normal);
        assert!(matches!(
            session.dispatch("/new", &mut console),
            Cmd::Continue
        ));
        assert_eq!(session.mode, tools::Mode::Explore);
        // The mode rides in the new history, once: a second `/new` starts with
        // the note it already has.
        let notes = |session: &Session| {
            session
                .messages
                .iter()
                .filter(|message| {
                    message["content"].as_str() == Some(&*tools::Mode::Explore.note())
                })
                .count()
        };
        assert_eq!(notes(&session), 1);
        assert!(matches!(
            session.dispatch("/new", &mut console),
            Cmd::Continue
        ));
        assert_eq!(notes(&session), 1);
    }

    #[test]
    fn a_skill_that_is_loaded_becomes_the_turn() {
        let dir = crate::state::testenv::dir("main-skill");
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: a demo\n---\nDo the demo.\n",
        )
        .unwrap();
        let loaded = skills::scan(&[(dir.clone(), skills::Scope::Project)]);
        let mut session = bare_session();
        session.skills = loaded.skills;
        let mut console = Console::Stdout;
        match session.dispatch("/skill:demo run it", &mut console) {
            Cmd::Turn(text) => {
                assert!(text.contains("Do the demo."), "{text}");
                assert!(text.ends_with("run it"), "{text}");
            }
            _ => panic!("a loaded skill is a turn"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compacting_replaces_the_history_with_one_summary() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"the summary"}}]}"#,
            "[DONE]",
        ])]);
        let mut session = session_at(&server);
        session.record(json!({ "role": "assistant", "content": "an answer" }));
        let before = session.messages.len();
        let mut console = Console::Stdout;

        assert!(session.compact(&mut console).is_none());
        assert_eq!(session.messages.len(), 3, "the base, then the summary");
        assert!(session.messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("the summary"));
        assert!(session.messages[2]["content"]
            .as_str()
            .unwrap()
            .starts_with(COMPACT_NOTE));
        assert_eq!(session.bytes, history_bytes(&session.messages));
        assert!(session.used.is_none(), "the old count is gone");
        assert_eq!(before, 3);
    }

    #[test]
    fn a_cut_summary_does_not_replace_the_history() {
        // The provider ended the summary at its own limit: what came back is
        // part of a summary, so the session keeps the history it had.
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"half a summary"}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            "[DONE]",
        ])]);
        let mut session = session_at(&server);
        session.record(json!({ "role": "assistant", "content": "an answer" }));
        let before = session.messages.clone();
        let mut console = Console::Stdout;

        assert!(session.compact(&mut console).is_none());
        assert_eq!(session.messages, before, "the session is unchanged");
        assert_eq!(session.bytes, history_bytes(&before));
    }

    #[test]
    fn a_compaction_asks_at_the_sessions_thinking_level_and_with_no_cap() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"the summary"}}]}"#,
            "[DONE]",
        ])]);
        let mut session = session_at(&server);
        session.record(json!({ "role": "assistant", "content": "an answer" }));
        session.thinking = Some("high".into());
        session.providers[0].cfg.thinking =
            vec![("high".into(), json!({ "reasoning_effort": "high" }))];
        let mut console = Console::Stdout;

        assert!(session.compact(&mut console).is_none());
        let body = server.text();
        assert!(body.contains(r#""reasoning_effort":"high""#), "{body}");
        assert!(body.contains(r#""temperature":0.0"#), "{body}");
        assert!(!body.contains("max_tokens"), "{body}");
    }

    #[test]
    fn a_reply_without_text_names_what_it_did_instead() {
        let reply = api::Reply {
            text: String::new(),
            reasoning: "thought".into(),
            calls: Vec::new(),
            usage: Some(api::Usage {
                prompt: 900,
                completion: 2000,
            }),
            finish_reason: Some("length".into()),
            complete: true,
        };
        assert_eq!(
            no_text(&reply),
            "finish reason: length, 2000 completion tokens, 7 chars of reasoning"
        );

        let bare = api::Reply {
            reasoning: String::new(),
            usage: None,
            finish_reason: None,
            ..reply
        };
        assert_eq!(no_text(&bare), "the provider sent no text");
    }

    #[test]
    fn a_session_with_nothing_answered_has_nothing_to_compact() {
        let mut session = bare_session();
        let mut console = Console::Stdout;
        assert!(session.compact(&mut console).is_none());
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn a_title_request_is_read_from_the_two_lines_it_asks_for() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"TITLE: A short title\n"}}]}"#,
            r#"{"choices":[{"delta":{"content":"SUMMARY: One sentence.\n"}}]}"#,
            "[DONE]",
        ])]);
        let provider = Provider {
            ep: server.ep.clone(),
            ..test_provider()
        };
        let text = title_request(&provider, "m", None, "the request", "the answer").unwrap();
        assert_eq!(
            parse_naming(&text),
            Some(("A short title".to_string(), "One sentence.".to_string()))
        );
        assert!(server
            .text()
            .contains("\"content\":\"User: the request\\nAssistant: the answer\""));
    }

    #[test]
    fn a_naming_reply_is_read_loosely_or_not_at_all() {
        assert_eq!(
            parse_naming("title: T\nsummary: S"),
            Some(("T".into(), "S".into()))
        );
        assert_eq!(
            parse_naming("TITLE: \"quoted\"\nSUMMARY: \"also quoted\""),
            Some(("quoted".into(), "also quoted".into()))
        );
        // A title with no summary is still a title.
        assert_eq!(
            parse_naming("TITLE: only"),
            Some(("only".into(), String::new()))
        );
        // No title, no name.
        assert_eq!(parse_naming("SUMMARY: orphan"), None);
        assert_eq!(parse_naming(""), None);
        assert_eq!(parse_naming("TITLE:\nSUMMARY: x"), None);
    }

    #[test]
    fn a_finished_title_is_applied_at_the_next_poll() {
        let dir = crate::state::testenv::dir("main-naming");
        let mut session = bare_session();
        session.store = Some(Store::fresh(dir.clone(), "abc123"));
        session.record(json!({ "role": "user", "content": "fix the parser" }));
        let (tx, rx) = mpsc::channel();
        tx.send(Some(("Named".to_string(), "Summary".to_string())))
            .unwrap();
        session.naming = Some(rx);
        let mut screen = ui::Screen::test_screen(60, 20);
        session.poll_naming(Some(&mut screen));
        let meta = session.store.as_ref().unwrap().meta.as_ref().unwrap();
        assert_eq!(meta.title, "Named");
        assert_eq!(meta.summary, "Summary");
        assert!(session.naming.is_none());

        // A poll with nothing to apply does nothing.
        session.poll_naming(None);
        // A thread that answered nothing ends the wait.
        let (tx, rx) = mpsc::channel();
        tx.send(None).unwrap();
        session.naming = Some(rx);
        session.poll_naming(None);
        assert!(session.naming.is_none());
        // So does one that died.
        let (tx, rx) = mpsc::channel::<Option<(String, String)>>();
        drop(tx);
        session.naming = Some(rx);
        session.poll_naming(None);
        assert!(session.naming.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_session_is_named_once_and_synchronously() {
        let server = Server::new(vec![sse(&[
            r#"{"choices":[{"delta":{"content":"TITLE: Made by hand\n"}}]}"#,
            "[DONE]",
        ])]);
        let dir = crate::state::testenv::dir("main-name-session");
        let mut session = session_at(&server);
        session.store = Some(Store::fresh(dir.clone(), "abc123"));
        session.record(json!({ "role": "user", "content": "a request" }));
        session.record(json!({ "role": "assistant", "content": "an answer" }));
        session.name_session();
        let meta = session.store.as_ref().unwrap().meta.as_ref().unwrap();
        assert_eq!(meta.title, "Made by hand");
        assert!(!session.may_name(), "named once is named");
        // A session with no store cannot be named at all.
        let mut bare = bare_session();
        bare.name_session();
        assert!(bare.store.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_resumed_history_is_replayed_into_the_transcript() {
        let mut session = bare_session();
        session.state.thoughts = true;
        session.messages = session.base_messages();
        session.messages[1] = json!({ "role": "user", "content": "explore mode: on" });
        // A compacted body opens with the summary. It was printed when it was
        // made; a resume starts from it, so the replay leaves it out.
        session.messages.push(json!({
            "role": "user",
            "content": format!("{COMPACT_NOTE}\n\nthe summary the model carries")
        }));
        session
            .messages
            .push(json!({ "role": "user", "content": "fix it" }));
        session.messages.push(json!({
            "role": "assistant",
            "content": "looking",
            "reasoning_content": "a thought",
            "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"x\"}" }
            }]
        }));
        session.messages.push(json!({
            "role": "tool",
            "tool_call_id": "c1",
            "content": "error: read x: no such file"
        }));
        let mut screen = ui::Screen::test_screen(60, 30);
        session.replay(&mut screen);
        let rows = screen.transcript();
        assert!(rows.iter().any(|row| row.contains("fix it")), "{rows:?}");
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("the summary the model carries")),
            "a resume does not print the summary again: {rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("looking")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("a thought")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("read")), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.contains("no such file")),
            "a failed call carries its reason: {rows:?}"
        );
    }

    #[test]
    fn a_long_session_replay_says_what_it_left_out() {
        let mut session = bare_session();
        for i in 0..110 {
            session
                .messages
                .push(json!({ "role": "user", "content": format!("line {i}") }));
        }
        let mut screen = ui::Screen::test_screen(60, 30);
        session.replay(&mut screen);
        let rows = screen.transcript();
        assert!(
            rows.iter().any(|row| row.contains("earlier messages")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("line 109")), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains("line 0")), "{rows:?}");
    }

    #[test]
    fn a_shell_line_joins_the_history_without_a_turn() {
        let mut session = bare_session();
        let result = session.run_shell("echo hi");
        assert!(result.contains("hi"), "{result}");
        let last = session.messages.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().starts_with("$ echo hi"));
    }

    #[test]
    fn the_banner_prints_a_plain_mark_when_the_terminal_is_narrow() {
        // No terminal in the test: the width is unknown and the block logo is
        // the fallback, and the notes follow it.
        startup::banner(&["a note".to_string()]);
        startup::banner(&[]);
    }

    fn down(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// One session on disk, under the directory `Store` keeps its bodies in.
    fn stored_session(dir: &std::path::Path, id: &str, title: &str, line: &str) {
        let mut meta = sessions::Meta::new(id);
        meta.title = title.to_string();
        meta.updated = 10;
        meta.save(dir).unwrap();
        let mut log = sessions::Log::open(dir, id).unwrap();
        log.append(&json!({ "role": "user", "content": line }))
            .unwrap();
    }

    #[test]
    fn a_shell_command_runs_behind_a_live_prompt() {
        let server = Server::new(Vec::new());
        let mut session = session_at(&server);
        let (_tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);

        let leftover = session.shell("echo sous", &mut screen, &rx);
        assert!(leftover.is_none());
        let last = session.messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let content = last["content"].as_str().unwrap();
        // The header sits on its own line under the command: the same bytes
        // whether the run had a screen or a pipe.
        assert!(content.starts_with("$ echo sous\nexit: 0"), "{content}");
        assert!(content.contains("sous"), "{content}");
        assert!(session.used.is_none(), "the count is an estimate again");
        let rows = screen.transcript();
        // The same line a tool call leaves: what ran, with one glyph, and the
        // command's own words under it — the `exit: 0` header the glyph has
        // just said is not repeated.
        assert!(
            rows.iter().any(|row| row.contains("▷ echo sous ✓")),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|row| row.contains("exit:")), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("sous")), "{rows:?}");
    }

    #[test]
    fn one_escape_offers_and_two_stop_a_command() {
        // One press asks, and the work is left alone.
        let cancel = http::Cancel::new();
        let (tx, rx) = events();
        tx.send(down(KeyCode::Esc)).unwrap();
        let mut screen = ui::Screen::test_screen(60, 20);
        thread::scope(|scope| {
            let handle = scope.spawn(|| 1u32);
            let (joined, queued) = pump(&mut screen, &rx, &cancel, handle);
            assert_eq!(joined, Some(1));
            assert!(queued.is_none());
        });
        assert!(!cancel.cancelled(), "one press offers, it does not stop");
        drop(tx);

        // A key between the presses withdraws the offer.
        let cancel = http::Cancel::new();
        let (tx, rx) = events();
        tx.send(down(KeyCode::Esc)).unwrap();
        tx.send(down(KeyCode::Char('q'))).unwrap();
        tx.send(down(KeyCode::Esc)).unwrap();
        let mut screen = ui::Screen::test_screen(60, 20);
        thread::scope(|scope| {
            let handle = scope.spawn(|| 1u32);
            pump(&mut screen, &rx, &cancel, handle);
        });
        assert!(!cancel.cancelled(), "the typing withdrew the offer");
        drop(tx);

        // Two presses in a row stop it.
        let cancel = http::Cancel::new();
        let (tx, rx) = events();
        tx.send(down(KeyCode::Esc)).unwrap();
        tx.send(down(KeyCode::Esc)).unwrap();
        let mut screen = ui::Screen::test_screen(60, 20);
        thread::scope(|scope| {
            let handle = scope.spawn(|| 1u32);
            pump(&mut screen, &rx, &cancel, handle);
        });
        assert!(cancel.cancelled());
        drop(tx);
    }

    #[test]
    fn the_line_loop_stops_only_with_nothing_left_and_no_input() {
        let mut pending = VecDeque::new();
        assert!(!done(&pending, false), "input is still there");
        assert!(done(&pending, true), "nothing queued and no input: done");
        pending.push_back("line".to_string());
        assert!(
            !done(&pending, true),
            "a line a turn handed back still runs"
        );
        assert!(!done(&pending, false));
    }

    #[test]
    fn end_of_input_stops_a_turn_and_says_so() {
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let cancel = http::Cancel::new();
        let mut screen = ui::Screen::test_screen(60, 20);
        let mut held = Held::default();
        let mut used = None;
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(held.event(Event::Key(key), &mut screen, &queue, &cancel, &mut used));
        assert!(cancel.cancelled(), "the running request is stopped");
        assert!(held.interrupted, "and the cut reply is announced");
    }

    #[test]
    fn a_failed_shell_command_keeps_its_words_and_loses_the_header() {
        let server = Server::new(Vec::new());
        let mut session = session_at(&server);
        let (_tx, rx) = events();
        let mut screen = ui::Screen::test_screen(60, 20);

        session.shell("exit 3", &mut screen, &rx);
        let rows = screen.transcript();
        assert!(
            rows.iter().any(|row| row.contains("▷ exit 3 ✗")),
            "{rows:?}"
        );
        assert!(!rows.iter().any(|row| row.contains("exit: 3")), "{rows:?}");
    }

    #[test]
    fn keys_and_work_share_the_prompt_while_a_command_runs() {
        let cancel = http::Cancel::new();
        let (tx, rx) = events();
        tx.send(down(KeyCode::Char('q'))).unwrap();
        tx.send(down(KeyCode::Char('p'))).unwrap();
        tx.send(Event::Paste("asted".into())).unwrap();
        tx.send(Event::Resize).unwrap();
        tx.send(down(KeyCode::Enter)).unwrap();
        tx.send(Event::Eof).unwrap();
        let mut screen = ui::Screen::test_screen(60, 20);

        let (joined, queued) = thread::scope(|scope| {
            let handle = scope.spawn(|| 7u32);
            pump(&mut screen, &rx, &cancel, handle)
        });
        assert_eq!(joined, Some(7));
        assert_eq!(queued.as_deref(), Some("qpasted"), "the line is kept");
        assert!(cancel.cancelled(), "the end of input cancels the work");
    }

    #[test]
    fn the_model_chooser_switches_to_the_row_enter_is_on() {
        let (tx, rx) = events();
        tx.send(down(KeyCode::Enter)).unwrap();
        let mut session = bare_session();
        session.providers[0].models.push(api::ModelInfo {
            id: "n".into(),
            context: Some(1),
        });
        let mut screen = ui::Screen::test_screen(60, 20);
        session.browse_models(&mut screen, &rx, "p/n", Scope::Dir);
        assert_eq!(session.model, "n");
        assert_eq!(screen.menu_selected(), 0, "the chooser closed");

        // A query that matches nothing is reported and then cleared.
        let (tx, rx) = events();
        tx.send(down(KeyCode::Enter)).unwrap();
        let mut screen = ui::Screen::test_screen(60, 20);
        session.browse_models(&mut screen, &rx, "nothing-matches", Scope::Dir);
        assert!(session.model == "n", "the earlier choice stands");
    }

    #[test]
    fn the_level_chooser_sets_and_remembers_a_level() {
        let dir = crate::state::testenv::dir("main-levels");
        let (tx, rx) = events();
        tx.send(down(KeyCode::Enter)).unwrap();
        let mut session = Session {
            state: state::State {
                cwd: dir.clone(),
                ..state::State::default()
            },
            ..bare_session()
        };
        session.providers[0].cfg.thinking = vec![
            ("off".into(), json!({})),
            ("high".into(), json!({ "reasoning_effort": "high" })),
        ];
        let mut screen = ui::Screen::test_screen(60, 20);
        session.browse_levels(&mut screen, &rx, "");
        assert_eq!(session.thinking.as_deref(), Some("off"));
        assert_eq!(
            session.state.thinking_for(&session.spec()).as_deref(),
            Some("off")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_session_chooser_resumes_what_it_is_on_and_closes_on_esc() {
        crate::state::testenv::home("main-browse-sessions", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = sessions::dir(&cwd);
            stored_session(&dir, "abc123", "the one", "earlier words");

            // Esc closes the chooser and leaves its rows in the transcript.
            let (tx, rx) = events();
            tx.send(down(KeyCode::Esc)).unwrap();
            let mut session = bare_session();
            session.state.cwd = cwd.clone();
            session.store = Some(Store::fresh(dir.clone(), "aaa111"));
            let mut screen = ui::Screen::test_screen(60, 20);
            session.browse_sessions(&mut screen, &rx, "");
            assert!(
                session.store.as_ref().unwrap().meta.is_none(),
                "nothing resumed"
            );

            // Enter resumes the selected session: its history replaces the
            // live one, and the screen replays it.
            let (tx, rx) = events();
            tx.send(down(KeyCode::Enter)).unwrap();
            let mut screen = ui::Screen::test_screen(60, 40);
            session.browse_sessions(&mut screen, &rx, "");
            assert_eq!(session.sid, "abc123");
            assert!(session
                .messages
                .iter()
                .any(|m| m["content"] == "earlier words"));
            let rows = screen.transcript();
            assert!(rows.iter().any(|row| row.contains("Resumed")), "{rows:?}");
            assert!(
                rows.iter().any(|row| row.contains("earlier words")),
                "{rows:?}"
            );

            // A directory with no sessions says so.
            let mut session = bare_session();
            session.state.cwd = home.join("empty");
            std::fs::create_dir_all(home.join("empty")).unwrap();
            let mut screen = ui::Screen::test_screen(60, 20);
            session.browse_sessions(&mut screen, &rx, "");
            let rows = screen.transcript();
            assert!(
                rows.iter().any(|row| row.contains("no sessions")),
                "{rows:?}"
            );

            // And no working directory at all is its own answer.
            let mut session = bare_session();
            session.browse_sessions(&mut screen, &rx, "");
            let rows = screen.transcript();
            assert!(
                rows.iter().any(|row| row.contains("working directory")),
                "{rows:?}"
            );
        });
    }

    #[test]
    fn a_piped_listing_numbers_what_it_shows_and_resumes_by_number() {
        crate::state::testenv::home("main-list-piped", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = sessions::dir(&cwd);
            stored_session(&dir, "abc123", "the first", "a needle");
            let mut session = bare_session();
            session.state.cwd = cwd.clone();
            session.store = Some(Store::fresh(dir.clone(), "aaa111"));

            session.list_piped("");
            assert_eq!(session.picks, vec!["abc123".to_string()]);
            session.list_piped("needle");
            assert_eq!(session.picks, vec!["abc123".to_string()]);
            session.list_piped("nothing");
            assert!(session.picks.is_empty(), "a search replaces the list");
            session.list_piped("");
            session.list_piped("1");
            assert_eq!(session.sid, "abc123", "the number resumes the row");
            assert!(session.messages.iter().any(|m| m["content"] == "a needle"));

            // A number that was never listed, and a directory with nothing in
            // it, are both answered rather than fatal.
            session.list_piped("9");
            let mut empty = bare_session();
            empty.state.cwd = home.join("empty");
            std::fs::create_dir_all(home.join("empty")).unwrap();
            empty.list_piped("");
            empty.list_piped("7");
            // No working directory: nothing to list at all.
            let mut bare = bare_session();
            bare.list_piped("");
        });
    }

    #[test]
    fn a_new_session_keeps_the_store_and_takes_a_new_id() {
        let dir = crate::state::testenv::dir("main-new");
        let mut session = bare_session();
        session.store = Some(Store::fresh(dir.clone(), "abc123"));
        session.sid = "abc123".into();
        session.providers[0].cfg.session_header = Some("x-session".into());
        session.providers[0]
            .headers
            .push(("x-session".into(), "abc123".into()));
        session.record(json!({ "role": "user", "content": "hi" }));
        let mut screen = ui::Screen::test_screen(60, 20);

        session.new_session(Some(&mut screen));
        assert_ne!(session.sid, "abc123");
        assert_eq!(session.messages.len(), 2, "the base, and nothing else");
        assert!(session.messages[0]["role"] == "system");
        assert_eq!(
            session.providers[0].headers[0],
            ("x-session".to_string(), session.sid.clone()),
            "the provider token follows the conversation"
        );
        assert_eq!(session.store.as_ref().unwrap().dir, dir, "same directory");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
