//! The REPL and session lifecycle: provider setup, the line loop, slash
//! commands, the agent turn, and the transcript formatting.

mod api;
mod complete;
mod config;
mod editor;
mod http;
mod markdown;
mod prompt;
mod render;
mod sandbox;
mod sessions;
mod skills;
mod state;
mod store;
mod text;
mod tools;
mod turn;
mod ui;
mod web;

use std::collections::VecDeque;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal;
use serde_json::{json, Value};

use prompt::{mode_note, system_with_instructions};
use render::{call_line, detail, failed, line_of, mark, plain_text, shell_line, Paint};
use store::{history_bytes, weight, Live, Store, COMPACT_NOTE};
use text::{strip_control, terminal_text};
use turn::{PlainSink, Queue, Sink, TextSink};

#[derive(Clone)]
struct Provider {
    name: String,
    cfg: config::ProviderConfig,
    ep: http::Endpoint,
    key: Option<String>,
    headers: Vec<(String, String)>,
    models: Vec<api::ModelInfo>,
}

fn main() {
    // Answered before anything else: no config, no network, no terminal.
    if std::env::args()
        .skip(1)
        .any(|arg| arg == "--version" || arg == "-V")
    {
        println!("axe {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let result = run();
    // The scratch directory is the one thing a session creates outside the
    // terminal; a static's `Drop` never runs, so this is the hook.
    sandbox::cleanup();
    if let Err(e) = result {
        eprintln!("axe: {}", strip_control(&e));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cfg = config::load()?;
    // `web_search` holds its token rather than the config: read once, like the
    // providers, and never re-read mid-session.
    web::set_brave_key(cfg.search.brave_key.clone());
    // Confinement is announced before anything runs, once, next to the
    // provider lines: weaker than promised must never be silent.
    if let Some(note) = sandbox::init() {
        eprintln!("axe: {}", strip_control(&note));
    }
    // Instructions are read once at startup, before any request: the system
    // prompt is a stable prefix and must not change mid-session.
    // Order: config `agents`, then the project's AGENTS.md, then `system`.
    let global_agents = cfg.agents.as_deref().and_then(config::read_agents);
    // The working directory identifies this directory's state file and anchors
    // the project's AGENTS.md; read it once.
    let cwd = std::env::current_dir().ok();
    let state = match &cwd {
        Some(dir) => state::load(dir),
        None => state::State::default(),
    };
    let project_file = cwd
        .as_deref()
        .and_then(config::find_agents)
        .filter(|f| Some(f.as_path()) != cfg.agents.as_deref());
    let project_agents = project_file.as_deref().and_then(config::read_agents);

    // Skills are read once, like the instructions: the catalogue is part of
    // the stable prefix.
    let loaded_skills = skills::load(cwd.as_deref(), &cfg.skills);
    // Explore mode's second writable directory, and the only one that outlives
    // the session: scratch is removed at exit, so a plan needs its own place.
    if let Some(dir) = &cwd {
        let plan = state::project(dir).join("plan");
        match state::private_dir(&plan) {
            Ok(()) => sandbox::set_plan(&plan),
            Err(e) => eprintln!("axe: plan directory: {e}"),
        }
    }
    for warning in &loaded_skills.warnings {
        eprintln!("axe: {}", strip_control(warning));
    }
    let project_skills = loaded_skills
        .skills
        .iter()
        .filter(|skill| skill.scope == skills::Scope::Project)
        .count();
    // Named once, next to the splash: which instructions and skills are in
    // force is part of the session's identity, not a warning.
    let mut notices = Vec::new();
    if let Some(file) = project_file.as_deref().filter(|_| project_agents.is_some()) {
        notices.push(format!(
            "Using project instructions from {}",
            file.display()
        ));
    }
    if project_skills > 0 {
        notices.push(format!(
            "Using {project_skills} project skill{}",
            plural(project_skills)
        ));
    }
    let mut providers = Vec::new();
    let sid = session_id();

    for pc in &cfg.providers {
        let ep = http::parse_url(&pc.base_url).map_err(|e| format!("{}: {e}", pc.name))?;
        let key = pc.resolved_key();
        if key.is_none() {
            if let Some(var) = &pc.api_key_env {
                eprintln!(
                    "{}: {} is not set, skipping",
                    strip_control(&pc.name),
                    strip_control(var)
                );
                continue;
            }
        }

        let mut headers = Vec::new();
        if let Some(name) = &pc.session_header {
            headers.push((name.clone(), sid.clone()));
        }

        let (models_ep, models_path) = match &pc.models_url {
            Some(url) => {
                let ep = http::parse_url(url).map_err(|e| format!("{}: {e}", pc.name))?;
                let path = if ep.prefix.is_empty() { "/models" } else { "" };
                (ep, path)
            }
            None => (ep.clone(), "/models"),
        };

        match api::list_models(&models_ep, models_path, key.as_deref(), &headers) {
            Ok(models) if !models.is_empty() => providers.push(Provider {
                name: pc.name.clone(),
                cfg: pc.clone(),
                ep,
                key,
                headers,
                models,
            }),
            Ok(_) => eprintln!("{}: no models", strip_control(&pc.name)),
            Err(e) => eprintln!("{}: {}", strip_control(&pc.name), strip_control(&e)),
        }
    }

    if providers.is_empty() {
        return Err("no models available".into());
    }

    // The directory's own choice wins; a directory with no state starts on the
    // config's `model`, which is the explicit default.
    let (provider, model) = resolve(&providers, state.model.as_deref().or(cfg.model.as_deref()));
    // Thinking level follows the model: it is remembered per `provider/id`,
    // first for this directory, then for the config, and turned into request
    // fields by `thinking_body`.
    let spec = format!("{}/{}", providers[provider].name, model);
    let thinking = state.thinking_for(&spec).or_else(|| {
        cfg.thinking
            .iter()
            .find(|(s, _)| *s == spec)
            .map(|(_, level)| level.clone())
    });
    validate_level(&providers[provider].cfg, thinking.as_deref());
    // An interactive run carries the selection in the live region's last row
    // and opens with a splash; a piped run owns no row and no screen, so the
    // banner is the only place the selection can appear.
    if ui::is_interactive() {
        banner(&notices);
    } else {
        // One leading blank: the same margin `ui::Screen` keeps on both edges.
        println!(
            " {}",
            terminal_text(&selection(&providers, provider, &model, &thinking))
        );
        for notice in &notices {
            println!(" {}", terminal_text(notice));
        }
    }

    // Built once. The mode lives in the history, not here, so a toggle leaves
    // the prefix byte-identical and the provider's cache intact. The project's
    // file is tagged: it is data about the repository, not authority.
    let skills_section = skills::prompt_section(&loaded_skills.skills);
    let system = system_with_instructions(
        global_agents.as_deref(),
        project_agents.as_deref().zip(project_file.as_deref()),
        cfg.system.as_deref(),
        Some(skills_section.as_str()).filter(|text| !text.is_empty()),
    );
    // The mode is history, not prompt: the config decides where a session
    // starts, and the note below tells the model which side it is on.
    let mode = if cfg.explore_by_default {
        tools::Mode::Explore
    } else {
        tools::Mode::Normal
    };
    let messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        mode_note(mode),
    ];

    // The directory's sessions: archive the old bodies before the listing so
    // the markers match what a chooser will resume.
    let mut startup = Vec::new();
    if let Some(dir) = &cwd {
        sessions::archive_old(dir, sessions::KEEP);
        startup = sessions::list(dir);
        startup.truncate(15);
    }
    let store = cwd
        .as_deref()
        .map(|dir| Store::fresh(sessions::dir(dir), &sid));

    let bytes = history_bytes(&messages);
    let session = Session {
        state,
        providers,
        provider,
        model,
        thinking,
        mode,
        used: None,
        messages,
        bytes,
        sid,
        store,
        startup,
        picks: Vec::new(),
        naming: None,
        warning: None,
        skills: loaded_skills.skills,
    };

    if ui::is_interactive() {
        session.run_interactive()
    } else {
        session.run_piped()
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
fn compact_request(
    provider: &Provider,
    model: &str,
    messages: &[Value],
    cancel: &http::Cancel,
) -> Result<String, String> {
    let extra = json!({ "max_tokens": 2000, "temperature": 0 });
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
    .map(|reply| reply.text)
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
            lines.push(format!("{role}: {}", cut(text, head, tail)));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args = call["function"]["arguments"].as_str().unwrap_or("");
                lines.push(format!("{role}: calls {name} {}", cut(args, 300, 100)));
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

/// `text` cut to `head` chars from the front and `tail` from the end, marked
/// where it was cut.
fn cut(text: &str, head: usize, tail: usize) -> String {
    let count = text.chars().count();
    if count <= head + tail + 1 {
        return text.to_string();
    }
    let front: String = text.chars().take(head).collect();
    let back: String = text.chars().skip(count - tail).collect();
    format!("{front}…[cut]…{back}")
}

/// One small completion, without tools. The session header stays: providers
/// that require it would refuse the request otherwise.
fn title_request(provider: &Provider, model: &str, user: &str, assistant: &str) -> Option<String> {
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
    let extra = json!({ "max_tokens": 200, "temperature": 0.3 });
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
    Model(Option<String>),
    /// None prints the current level; Some sets it.
    Thinking(Option<String>),
    Thoughts,
    Explore,
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
        "/model" => return Command::Model(None),
        "/thinking" => return Command::Thinking(None),
        "/thoughts" => return Command::Thoughts,
        "/explore" => return Command::Explore,
        "/sessions" => return Command::Sessions(None),
        "/skills" => return Command::Skills,
        _ => {}
    }
    if let Some(rest) = input.strip_prefix("/model ") {
        return Command::Model(Some(rest.trim().to_string()));
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

/// What the prompt yielded: a line, or a mode toggle that is not one.
enum Input {
    /// The committed line.
    Line(String),
    /// Shift+Tab: explore mode, handled without touching the history.
    Explore,
    /// Enter on a chooser: the selected row.
    Menu(usize),
    /// Esc on a chooser.
    Cancel,
}

/// Slash commands, for Tab completion in the prompt. Only the name is
/// completed; `/model` and `/thinking` take their argument as typed.
const COMMANDS: &[&str] = &[
    "/compact",
    "/exit",
    "/explore",
    "/help",
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

const HELP: &str = "/model [id]    list the models, or switch to one\n\
                    /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                    /thoughts      show or hide the model's reasoning\n\
                    /sessions [q]  list, search and resume sessions\n\
                    /compact       replace the history with a summary of itself\n\
                    /skills        list loaded skills (project, global, config)\n\
                    /skill:name [request]  load a skill's instructions and run a turn\n\
                    /new           start a new session\n\
                    /explore       toggle explore mode (read-only shell)\n\
                    !cmd           run a shell command; its output joins the context\n\
                    /help          list the commands and the config keys\n\
                    /exit, /quit   quit\n";

/// `/help`: the command list, then where the config is read from and the keys
/// it takes. The path is resolved rather than written out, because
/// `XDG_CONFIG_HOME` moves it.
fn help_text() -> String {
    format!(
        "{HELP}\nconfig: {}\n\
         keys: providers, model, explore_by_default, search, thinking, system, agents, skills\n\
         provider keys: base_url, api_key_env, models_url, session_header, thinking",
        config::path().display()
    )
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
        spawn_input(tx.clone());

        // A line submitted mid-turn arrives here when the turn could not take
        // it: a command, which needs the session and the screen, or a request
        // that missed every round boundary. It is dispatched exactly like a
        // line typed while idle, which is why slash commands are handled in
        // this loop and not only where they are parsed.
        let mut pending: Option<String> = None;
        loop {
            self.poll_naming(Some(&mut screen));
            let (line, was_queued) = match pending.take() {
                Some(line) => (line, true),
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
                    pending = self.messages_turn(&input, &tx, &rx, &mut screen)?;
                }
                Cmd::Shell(command) => {
                    pending = self.shell(&command, &mut screen, &rx);
                }
                Cmd::Compact => {
                    pending = self.compact(&mut Console::Screen {
                        screen: &mut screen,
                        rx: &rx,
                    });
                }
            }
            self.flush_warnings(Some(&mut screen));
        }
        self.finalize_session();
        self.flush_warnings(Some(&mut screen));
        screen.close();
        Ok(())
    }

    /// No tty: the kernel edits the line and the turn prints as it streams.
    fn run_piped(mut self) -> Result<(), String> {
        while let Some(line) = ui::cooked_line("❯ ") {
            let cmd = match line.trim() {
                "" => Cmd::Continue,
                _ => self.dispatch(&line, &mut Console::Stdout),
            };
            match cmd {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    let mut sink = PlainSink::default();
                    let cancel = http::Cancel::new();
                    self.record(json!({ "role": "user", "content": input }));
                    if let Err(e) = self.turn(&mut sink, &cancel) {
                        eprintln!("axe: {}", strip_control(&e));
                    }
                    self.name_session();
                }
                Cmd::Shell(command) => {
                    // The user's own shell, and the same glyph the screen
                    // draws: `!cmd` is never confined, so the line is the
                    // unconfined color.
                    let mut console = Console::Stdout;
                    console.output(&format!("{}\n", plain_text(&shell_line(&command, false))));
                    let result = self.run_shell(&command);
                    console.output(&result);
                }
                Cmd::Compact => {
                    self.compact(&mut Console::Stdout);
                }
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
    /// `provider/id` (state first, then config), persist the selection, and
    /// return the new info row.
    fn set_model(&mut self, provider: usize, id: String) -> Vec<(ui::Info, String)> {
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
        self.state.set_model(&spec);
        if let Err(e) = self.state.save() {
            self.warn(format!("state: {e}"));
        }
        self.info()
    }

    /// Apply a model choice and say so: the info row, and the line the
    /// transcript keeps.
    fn choose_model(&mut self, provider: usize, id: String, console: &mut Console) {
        let info = self.set_model(provider, id);
        console.output(&format!("{}\n", ui::info_text(&info)));
        console.info(info);
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
        console.output(&format!("thinking: {label}\n"));
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
            screen.output(&format!("Resumed: {title}\n"));
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
            screen.output(&format!("… {start} earlier messages\n"));
        }
        for message in &history[start..] {
            match message["role"].as_str() {
                Some("user") => {
                    if let Some(text) = message["content"].as_str() {
                        screen.echo_user(text);
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
        let fallback = sessions::fallback(&self.messages);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let named = title_request(&provider, &model, &user, &assistant)
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
        let named = title_request(provider, &model, &user, &assistant)
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
            screen.output("sessions need a working directory\n");
            return;
        }
        let metas = sessions::list(&self.state.cwd);
        if metas.is_empty() {
            screen.output("No sessions in this directory.\n");
            return;
        }
        let mut query = query.trim().to_string();
        loop {
            let found: Vec<(usize, String)> = if query.is_empty() {
                (0..metas.len())
                    .map(|index| (index, String::new()))
                    .collect()
            } else {
                sessions::search(&self.state.cwd, &metas, &query)
                    .into_iter()
                    .map(|hit| (hit.index, hit.snippet))
                    .collect()
            };
            if found.is_empty() {
                screen.output(&format!("No sessions match {query:?}\n"));
                query.clear();
                continue;
            }
            let mut ids = Vec::new();
            let mut rows = Vec::new();
            for (index, snippet) in &found {
                let meta = &metas[*index];
                ids.push(meta.id.clone());
                let mut row = session_row(meta);
                if !snippet.is_empty() {
                    row.push_str("  ·  ");
                    row.push_str(snippet);
                }
                rows.push(row);
            }
            let header = if query.is_empty() {
                "Sessions (↑↓ · Enter; type to search; Esc to close)".to_string()
            } else {
                format!("Sessions matching {query:?} (↑↓ · Enter; type to refine; Esc)")
            };
            screen.set_menu(&header, rows, false);
            match readline(screen, rx, &mut |_| {}) {
                Some(Input::Menu(index)) => {
                    let Some(id) = ids.get(index).cloned() else {
                        continue;
                    };
                    screen.clear_menu(false);
                    match self.resume(&id, Some(screen)) {
                        Ok(_) => return,
                        Err(e) => {
                            screen.error(&format!("axe: {e}\n"));
                            return;
                        }
                    }
                }
                Some(Input::Cancel) | None => {
                    screen.clear_menu(true);
                    return;
                }
                Some(Input::Line(line)) => {
                    query = line.trim().to_string();
                }
                Some(Input::Explore) => continue,
            }
        }
    }

    /// `/model`: a chooser over every provider's models, opened on the one in
    /// use. Typing narrows the list, Enter switches, Esc closes.
    fn browse_models(&mut self, screen: &mut ui::Screen, rx: &Receiver<Event>, query: &str) {
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
        let on = items.iter().position(|(spec, _)| *spec == self.spec());
        let Some(index) = choose(screen, rx, "Models", &items, on, query) else {
            return;
        };
        let Some((provider, id)) = find(&self.providers, &items[index].0) else {
            return;
        };
        let mut console = Console::Screen { screen, rx };
        self.choose_model(provider, id, &mut console);
    }

    /// `/thinking`: a chooser over the levels this provider configures. A
    /// provider with one level has nothing to pick and never gets here.
    fn browse_levels(&mut self, screen: &mut ui::Screen, rx: &Receiver<Event>, query: &str) {
        let names = level_names(&self.providers[self.provider].cfg);
        let items: Vec<(String, String)> = names
            .iter()
            .map(|name| (name.clone(), name.clone()))
            .collect();
        let on = names
            .iter()
            .position(|name| Some(name) == self.thinking.as_ref());
        let Some(index) = choose(screen, rx, "Thinking levels", &items, on, query) else {
            return;
        };
        let level = names[index].clone();
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
                Ok(title) => println!("Resumed: {}", terminal_text(&title)),
                Err(e) => eprintln!("axe: {}", strip_control(&e)),
            }
            return;
        }

        let all = sessions::list(&self.state.cwd);
        if all.is_empty() {
            println!("no sessions");
            self.picks.clear();
            return;
        }
        let selected: Vec<(sessions::Meta, String)> = if arg.trim().is_empty() {
            all.into_iter().map(|meta| (meta, String::new())).collect()
        } else {
            sessions::search(&self.state.cwd, &all, arg)
                .into_iter()
                .map(|hit| (all[hit.index].clone(), hit.snippet))
                .collect()
        };
        if selected.is_empty() {
            println!("no sessions match {:?}", arg.trim());
            self.picks.clear();
            return;
        }
        self.picks = selected.iter().map(|(meta, _)| meta.id.clone()).collect();
        for (at, (meta, snippet)) in selected.iter().enumerate() {
            let mut row = session_row(meta);
            if !snippet.is_empty() {
                row.push_str("  ·  ");
                row.push_str(snippet);
            }
            println!("{:>2}) {}", at + 1, terminal_text(&row));
        }
    }

    /// The live region's last row: what this session is pointed at, how much of
    /// the window it has filled, and the mode when that changes the tool set.
    /// One piece per thing, so the screen can color each on its own.
    fn info(&self) -> Vec<(ui::Info, String)> {
        let mut parts = Vec::new();
        if let Some(title) = self.title() {
            parts.push((ui::Info::Title, title));
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
    }

    /// The messages every history starts with: the built system prompt and the
    /// current mode note. The prefix itself is never rewritten.
    fn base_messages(&self) -> Vec<Value> {
        let system = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| json!({ "role": "system", "content": tools::system_prompt() }));
        vec![system, mode_note(self.mode)]
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
            out.push_str("no skills\n");
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
        screen.insert(vec![line_of(&shell_line(command, false))], ui::Block::Tool);
        screen.set_status("Running…");
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
        screen.output(&result);
        // The output is part of the history now: drop the provider's last
        // count so the fill estimate includes it.
        self.used = None;
        self.record(json!({
            "role": "user",
            "content": format!("$ {command}
        {result}"),
        }));
        screen.set_info(self.info());
        screen.repaint();
        queued
    }

    /// `/compact`: ask the model to summarize the session, then continue from
    /// the summary. The body is not rewritten — the summary is appended as a
    /// marked user message and the loader starts replay from it.
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
            console.output("nothing to compact\n");
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
        let mut queued = None;
        let result = match console {
            Console::Screen { screen, rx } => {
                screen.set_status("Compacting…");
                screen.repaint();
                let result = thread::scope(|scope| {
                    let handle =
                        scope.spawn(|| compact_request(provider, &model, &request, &cancel));
                    let (joined, line) = pump(screen, rx, &cancel, handle);
                    queued = line;
                    joined.unwrap_or_else(|| Err("compaction thread panicked".into()))
                });
                screen.set_status("");
                result
            }
            Console::Stdout => compact_request(provider, &model, &request, &cancel),
        };

        match result {
            Ok(text) if !text.trim().is_empty() => {
                let summary = text.trim().to_string();
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
                console.output(&format!("compacted {replaced} messages into 1 summary\n"));
            }
            Ok(_) => console.error("compaction: the model returned nothing"),
            Err(_) if cancel.cancelled() => console.output("Compaction cancelled\n"),
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
    ) -> Result<Option<String>, String> {
        self.record(json!({ "role": "user", "content": input }));
        // The first request is the session's name until the model gives it one:
        // show it now, not once the turn is over.
        screen.set_info(self.info());

        let cancel = http::Cancel::new();
        let mut queued: Option<String> = None;
        // A line typed while this turn runs, handed to the turn's own thread so
        // that it joins the history at the next round boundary instead of after
        // the last one. A command cannot go that way and waits for the end.
        let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));
        let turn_queue = queue.clone();
        // Streamed text up to the last newline is permanent; the rest is the
        // line still being written and lives in the status row.
        let mut partial = String::new();
        // Reasoning before the answer. Its last line is unfinished until the
        // answer starts or the round ends, so it is held until then.
        let mut thought = String::new();
        // A mode switch pressed while the turn runs. Shift+Tab reached the live
        // prompt and used to be dropped; it is held here instead, because the
        // turn's tool set and history are fixed before it starts.
        let mut switch = false;
        // ESC asks first: one press offers the interrupt, the next one takes it.
        let mut armed = false;
        // Whether the turn was actually stopped, for the line that says so.
        let mut interrupted = false;

        screen.set_status("Thinking…");
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
                let mut done = false;
                let Some(first) = first else {
                    continue;
                };
                let mut next = Some(first);
                while let Some(ev) = next.take().or_else(|| rx.try_recv().ok()) {
                    match ev {
                        // ESC is the interrupt, and it takes two presses: the
                        // first offers it and any other key withdraws the
                        // offer, so a mistyped ESC cannot end a turn that was
                        // nearly done. The flag ends the turn between
                        // requests; the socket shutdown unblocks the one in
                        // flight.
                        Event::Key(key) if key.code == KeyCode::Esc => {
                            if armed {
                                interrupted = true;
                                cancel.cancel();
                                screen.set_alert(Some("Interrupting…"));
                            } else {
                                armed = true;
                                screen.set_alert(Some("press ESC again to interrupt"));
                            }
                        }
                        Event::Key(key) => {
                            // A key other than ESC withdraws the offer.
                            if armed {
                                armed = false;
                                screen.set_alert(None);
                            }
                            match screen.apply_key(key) {
                                ui::Action::Eof => {
                                    cancel.cancel();
                                    done = true;
                                }
                                ui::Action::Submit => {
                                    if !screen.is_empty() && queued.is_none() {
                                        // Queue without echoing here: the
                                        // transcript shows the line where the
                                        // model saw it, which is the round
                                        // boundary the turn takes it at, or
                                        // the turn's end for a command.
                                        let line = screen.take();
                                        // A plain request reaches the model
                                        // between rounds; a command needs the
                                        // session and the screen, so it stays
                                        // for the boundary in the REPL loop.
                                        if let Command::Turn(input) = parse_command(&line) {
                                            if let Ok(mut lines) = queue.lock() {
                                                lines.push_back(input);
                                            }
                                        }
                                        queued = Some(line);
                                        screen.set_queued(queued.as_deref());
                                    }
                                }
                                // The turn already captured its tool set and its
                                // history prefix; the switch is applied at the
                                // boundary, below, so the note lands after the
                                // answer it did not affect.
                                ui::Action::Explore => switch = true,
                                // A chooser only exists while idle.
                                ui::Action::MenuStep(_)
                                | ui::Action::MenuAccept
                                | ui::Action::MenuCancel => {}
                                ui::Action::None => {}
                            }
                        }
                        Event::Paste(text) => screen.insert_text(&text),
                        // The turn took a queued line into its history: it is
                        // no longer pending, and it belongs in the transcript
                        // at the point the model saw it.
                        Event::User(line) => {
                            queued = None;
                            screen.set_queued(None);
                            screen.echo_user(&line);
                        }
                        // Model text, already spaced by the sink: everything up
                        // to the last newline is a finished line.
                        Event::Printed(text) => {
                            // The answer starting is the end of the thought: its
                            // unfinished last line goes out first.
                            let tail = std::mem::take(&mut thought);
                            screen.thought(&tail);
                            partial.push_str(&text);
                            // Lines end at a newline; blank lines at the tail
                            // are the separator before whatever comes next, so
                            // they wait. A chunk boundary cannot then split one
                            // paragraph break into two.
                            let tail = partial.len() - partial.trim_end_matches('\n').len();
                            if tail > 0 {
                                let end = partial.len() - (tail - 1);
                                let finished: String = partial.drain(..end).collect();
                                if !finished.is_empty() {
                                    screen.text(&finished);
                                }
                            }
                            // The unfinished line is not the status: the row
                            // says what the turn is doing, not what it says.
                            screen.set_status("Responding…");
                        }
                        // A tool call and its outcome, on one line.
                        Event::Echo(spans, error) => {
                            // A round that only reasoned and called a tool: its
                            // thought ends here, before the call it produced.
                            let tail = std::mem::take(&mut thought);
                            screen.thought(&tail);
                            // The command that was running is in the transcript
                            // now; the status row goes back to being empty.
                            screen.set_status("");
                            screen.insert(vec![line_of(&spans)], ui::Block::Tool);
                            if let Some(error) = &error {
                                screen.fail(error);
                            }
                        }
                        // Reasoning, dim: it precedes the answer and is not the
                        // answer, and it must not disturb the status row.
                        Event::Thought(text) => {
                            thought.push_str(&text);
                            let finished = match thought.rfind('\n') {
                                Some(at) => thought.drain(..=at).collect::<String>(),
                                None => String::new(),
                            };
                            screen.thought(&finished);
                        }
                        Event::Status(text) => {
                            if screen.status() != text {
                                screen.set_status(&text);
                            }
                        }
                        // The area under us changed: paint even if no state
                        // did.
                        Event::Resize => screen.invalidate(),
                        Event::Done(Err(e)) => {
                            screen.error(&format!("axe: {e}\n"));
                            done = true;
                        }
                        Event::Done(Ok(count)) => {
                            used = count;
                            let tail = std::mem::take(&mut thought);
                            screen.thought(&tail);
                            // The last block has no ending line of its own.
                            screen.flush();
                            partial.clear();
                            done = true;
                        }
                        Event::Eof => {
                            cancel.cancel();
                            done = true;
                        }
                    }
                    // An interrupt does not break the loop: the turn thread
                    // still reports what the killed command did, and `Done`
                    // is the only event that ends the drain.
                    if done {
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
        if interrupted {
            // No emoji-presentation glyph here: `unicode-width` calls those one
            // cell while the terminal may draw two, and the space after it is
            // the cost.
            screen.error("Turn interrupted — the reply is partial\n");
        }
        screen.set_queued(None);
        // A turn that reported nothing (interrupted, or a provider that does
        // not count) leaves the estimate in charge rather than a stale number.
        self.used = used;
        screen.set_info(self.info());

        // Held from the turn: applied now, so the note sits below the answer
        // and above whatever the user types next.
        if switch {
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
        // that arrived too late for one. A command line never entered the
        // queue, and there is only ever one line pending.
        let leftover = queue.lock().ok().and_then(|mut lines| lines.pop_front());
        Ok(leftover.or(queued))
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
                self.new_session(console.screen());
                console.output("New session\n");
                Cmd::Continue
            }
            Command::Model(None) => {
                match console {
                    Console::Screen { screen, rx } => self.browse_models(screen, rx, ""),
                    Console::Stdout => console.output(&models(&self.providers)),
                }
                Cmd::Continue
            }
            Command::Model(Some(spec)) => {
                match find(&self.providers, &spec) {
                    Some((provider, id)) => self.choose_model(provider, id, console),
                    // On a screen an argument that names no model is a search,
                    // not a mistake: the chooser opens narrowed to it.
                    None => match console {
                        Console::Screen { screen, rx } => self.browse_models(screen, rx, &spec),
                        Console::Stdout => console.error(&format!("unknown model: {spec}")),
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
                        _ => console.output(&format!("thinking: {label}\n")),
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
                        console.error(&format!("unknown skill: {name} (see /skills)"));
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

/// Events from the two producers that never draw: keys, and the turn.
enum Event {
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
fn spawn_input(tx: Sender<Event>) {
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

/// How often the idle prompt looks for work that finished off the UI thread.
const IDLE: Duration = Duration::from_millis(100);

/// Wait for work another thread is doing while the prompt stays live: keys are
/// folded into the line, Esc asks the cancel for it, and the loop ends when the
/// work is finished. What the thread joined to comes back with the one line
/// typed while it ran — a panic is `None`, and the caller names it.
///
/// `!cmd` and `/compact` wait the same way, which is why this is one function:
/// the two used to be one loop each, and only one of them took a line.
fn pump<T>(
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
                screen.set_status("Interrupting…");
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
fn readline(
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

/// The items a query keeps: a case-insensitive substring of the spec, which is
/// what typing matches, never the row with its context window.
fn narrow(items: &[(String, String)], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, (spec, _))| spec.to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}

/// Walk a chooser until Enter, Esc or the end of input: the arrows move, a
/// typed line narrows the list, Enter takes a row. `items` are (spec, row) — a
/// query matches the spec, the screen draws the row — and the answer is the
/// index into `items` that Enter chose. `on` is where the marker starts, so the
/// chooser opens on what the session already uses.
fn choose(
    screen: &mut ui::Screen,
    rx: &Receiver<Event>,
    title: &str,
    items: &[(String, String)],
    on: Option<usize>,
    query: &str,
) -> Option<usize> {
    let mut query = query.trim().to_string();
    loop {
        let found = narrow(items, &query);
        if found.is_empty() {
            screen.output(&format!("No {title} match {query:?}\n"));
            query.clear();
            continue;
        }
        let rows: Vec<String> = found.iter().map(|index| items[*index].1.clone()).collect();
        let header = if query.is_empty() {
            format!("{title} (↑↓ · Enter; type to narrow; Esc)")
        } else {
            format!("{title} matching {query:?} (↑↓ · Enter; type to refine; Esc)")
        };
        screen.set_menu(&header, rows, false);
        if let Some(at) = on.and_then(|index| found.iter().position(|hit| *hit == index)) {
            screen.menu_select(at);
        }
        match readline(screen, rx, &mut |_| {}) {
            Some(Input::Menu(at)) => {
                screen.clear_menu(false);
                return found.get(at).copied();
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
        if self.thoughts {
            self.send(Event::Thought(text.to_string()));
        } else {
            // The thought is hidden, but not its cost: its first chunk starts
            // the clock, and `end_thought` puts the duration in its place.
            self.thought.get_or_insert_with(Instant::now);
            self.send(Event::Status("Thinking…".to_string()));
        }
    }
}

fn thinking_label(provider: &config::ProviderConfig, level: &Option<String>) -> String {
    match level {
        Some(level) => format!("{level}  (levels: {})", level_names(provider).join(", ")),
        None => "default (not set)".to_string(),
    }
}

fn level_names(provider: &config::ProviderConfig) -> Vec<String> {
    let mut names: Vec<String> = provider.thinking.iter().map(|(k, _)| k.clone()).collect();
    if names.is_empty() {
        names.push("off".to_string());
    }
    names
}

/// Map user input onto a configured level name. Returns the canonical name, so
/// the config never stores an alias.
fn set_thinking(provider: &config::ProviderConfig, input: &str) -> Result<String, String> {
    let names = level_names(provider);
    names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(input))
        .cloned()
        .ok_or_else(|| {
            format!(
                "unknown thinking level: {input} (known: {})",
                names.join(", ")
            )
        })
}

/// Warn once at startup if the config remembers a level the provider dropped.
fn validate_level(provider: &config::ProviderConfig, level: Option<&str>) {
    let Some(level) = level else { return };
    if !level_names(provider).iter().any(|name| name == level) {
        eprintln!(
            "axe: {}: thinking level \"{level}\" is not configured, using default",
            provider.name
        );
    }
}

fn selection(
    providers: &[Provider],
    provider: usize,
    model: &str,
    thinking: &Option<String>,
) -> String {
    format!(
        "{}{}{}",
        place(providers, provider, model),
        ui::SEP,
        level(thinking)
    )
}

/// `provider/model`, the pair that answers a turn.
fn place(providers: &[Provider], provider: usize, model: &str) -> String {
    format!("{}/{}", providers[provider].name, model)
}

fn level(thinking: &Option<String>) -> String {
    thinking.clone().unwrap_or_else(|| "default".to_string())
}

/// The window as a whole percent, saturating: a count that overshoots the
/// window is full, not 140%.
fn fullness(used: u64, ctx: u64) -> u8 {
    (used.saturating_mul(100) / ctx.max(1)).min(100) as u8
}

/// Token counts in three characters where possible: `812`, `34k`, `1.0M`.
fn scaled(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Whole percent, with `<1%` for a history that is started but rounds to zero:
/// true on a million-token window, and `0%` would read as empty.
fn percent(used: u64, ctx: u64) -> String {
    let ctx = ctx.max(1);
    match used * 100 / ctx {
        0 if used > 0 => "<1%".to_string(),
        n => format!("{n}%"),
    }
}

/// Splash, printed once above the live region: block letters for `axe`. It
/// lands in the transcript and is never repainted, so it must be narrower than
/// the terminal's margin-to-margin width on a narrow window.
const LOGO: &[&str] = &[" ▄▄▄  ▄▄ ▄▄ ▄▄▄▄▄", "██▀██ ▀█▄█▀ ██▄▄", "██▀██ ██ ██ ██▄▄▄"];

fn banner(notes: &[String]) {
    // One leading blank: the same margin `ui::Screen` keeps on both edges.
    println!();
    let width = terminal::size().map(|(width, _)| width).unwrap_or(0);
    let logo_width = LOGO
        .iter()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0);
    // The block logo must fit between the margins; a narrow terminal gets the
    // one-word mark instead of a wrapped banner.
    let logo: &[&str] = if width == 0 || width > logo_width {
        LOGO
    } else {
        &["axe"]
    };
    // The release rides the mark's second row; the one-word fallback has no
    // second row, so it takes the one it has.
    let version_row = (logo.len() - 1).min(1);
    for (i, line) in logo.iter().enumerate() {
        let mut out = Paint::Cyan.ansi(line);
        if i == version_row {
            out.push_str("   ");
            out.push_str(&Paint::Dim.ansi(env!("CARGO_PKG_VERSION")));
        }
        println!(" {}", out);
    }
    if !notes.is_empty() {
        println!();
        for note in notes {
            println!(" {}", Paint::Dim.ansi(note));
        }
    }
    println!();
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn session_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

fn models(providers: &[Provider]) -> String {
    let mut text = String::new();
    for p in providers {
        text.push_str(&format!("{}:\n", p.name));
        for m in &p.models {
            match m.context {
                Some(ctx) => text.push_str(&format!("  {}/{}  ({} ctx)\n", p.name, m.id, ctx)),
                None => text.push_str(&format!("  {}/{}\n", p.name, m.id)),
            }
        }
    }
    text
}

fn find(providers: &[Provider], spec: &str) -> Option<(usize, String)> {
    if let Some((pname, mid)) = spec.split_once('/') {
        if let Some(pi) = providers.iter().position(|p| p.name == pname) {
            if providers[pi].models.iter().any(|m| m.id == mid) {
                return Some((pi, mid.to_string()));
            }
        }
    }
    for (pi, p) in providers.iter().enumerate() {
        if p.models.iter().any(|m| m.id == spec) {
            return Some((pi, spec.to_string()));
        }
    }
    None
}

fn resolve(providers: &[Provider], spec: Option<&str>) -> (usize, String) {
    if let Some(spec) = spec {
        if let Some(hit) = find(providers, spec) {
            return hit;
        }
        eprintln!("axe: model {spec:?} not available, using default");
    }
    (0, providers[0].models[0].id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::render::summarize;

    /// The info row as one line: the screen joins the pieces, and so does
    /// anything that asserts on them.
    fn info_text(session: &Session) -> String {
        ui::info_text(&session.info())
    }

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
        assert_eq!(narrow(&items, ""), vec![0, 1, 2]);
        assert_eq!(narrow(&items, "DEEP"), vec![0]);
        assert_eq!(narrow(&items, "local"), vec![1]);
        assert_eq!(narrow(&items, "go/"), vec![2]);
        // The context window is drawn, not searched.
        assert_eq!(narrow(&items, "ctx"), Vec::<usize>::new());
        assert_eq!(narrow(&items, "open/code"), Vec::<usize>::new());
    }

    #[test]
    fn control_bytes_are_stripped_but_lines_survive() {
        assert_eq!(strip_control("a\x1b[31mb\nc\td\u{9b}"), "a[31mb\nc\td");
    }

    #[test]
    fn parses_commands_once() {
        assert_eq!(parse_command("  "), Command::Empty);
        assert_eq!(parse_command("/model"), Command::Model(None));
        assert_eq!(
            parse_command("/model a/b"),
            Command::Model(Some("a/b".into()))
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
        assert_eq!(cut("abc", 2, 2), "abc");
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
    fn display_helpers() {
        assert_eq!(scaled(9999), "9999");
        assert_eq!(scaled(10_000), "10k");
        assert_eq!(scaled(1_000_000), "1.0M");
        assert_eq!(percent(1, 1000), "<1%");
        assert_eq!(percent(50, 100), "50%");
        assert!(failed("bash", "exit: 1\nboom"));
        assert!(!failed("bash", "exit: 0\nok"));
        assert!(failed("tools", "error: nope"));
        assert_eq!(mark("bash", "exit: 0\n").1, "✓");
        assert_eq!(mark("bash", "exit: 1\n").1, "✗");
        assert_eq!(detail("exit: 1\nboom\nmore"), "boom…");
        assert_eq!(summarize("a\nb", 10), "a…");
    }

    #[test]
    fn session_ids_are_32_hex_chars() {
        let id = session_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
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
            find(std::slice::from_ref(&provider), "p/m"),
            Some((0, "m".into()))
        );
        assert_eq!(
            find(std::slice::from_ref(&provider), "m"),
            Some((0, "m".into()))
        );
        assert_eq!(
            resolve(std::slice::from_ref(&provider), Some("p/m")),
            (0, "m".into())
        );
        assert_eq!(
            resolve(std::slice::from_ref(&provider), Some("missing")),
            (0, "m".into())
        );
        assert_eq!(selection(&[provider], 0, "m", &None), "p/m  ·  default");
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
        let info = ui::info_text(&session.set_model(0, "m".into()));
        assert!(info.contains("p/m"), "{info}");
        assert_eq!(session.used, None);
        assert_eq!(session.state.model.as_deref(), Some("p/m"));
        session.new_session(None);
        assert!(session.picks.is_empty());
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0]["content"].as_str(), Some("s"));
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
}
