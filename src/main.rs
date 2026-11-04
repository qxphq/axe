//! The REPL and session lifecycle: provider setup, the line loop, slash
//! commands, the agent turn, and the transcript formatting.

mod api;
mod config;
mod http;
mod markdown;
mod sandbox;
mod sessions;
mod state;
mod tools;
mod ui;
mod web;

use std::cell::RefCell;
use std::io::{self, IsTerminal, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEventKind};
use ratatui::crossterm::terminal;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{json, Value};

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
    let result = run();
    // The scratch directory is the one thing a session creates outside the
    // terminal; a static's `Drop` never runs, so this is the hook.
    sandbox::cleanup();
    if let Err(e) = result {
        eprintln!("nano: {}", strip_control(&e));
        std::process::exit(1);
    }
}

/// The system prompt: built-in rules, then instructions. `global` and the
/// config's `system` are the user's own text; the project's `AGENTS.md` comes
/// from a repository and is wrapped in a tag that marks it as data.
fn system_with_instructions(
    global: Option<&str>,
    project: Option<(&str, &Path)>,
    extra: Option<&str>,
) -> String {
    let mut instructions = String::new();
    if let Some(text) = global {
        instructions.push_str("\n\n# Instructions (global)\n\n");
        instructions.push_str(text);
    }
    if let Some((text, path)) = project {
        instructions.push_str(&format!(
            "\n\n<project_instructions path=\"{}\">\n{text}\n</project_instructions>",
            tag_path(&path.display().to_string())
        ));
    }
    if let Some(text) = extra {
        instructions.push_str("\n\n");
        instructions.push_str(text);
    }
    let mut system = tools::system_prompt();
    system.push_str(&instructions);
    system
}

/// A path inside the tag's attribute: a quote or a control byte would end or
/// break the attribute, so both become an apostrophe.
fn tag_path(path: &str) -> String {
    path.chars()
        .map(|c| if c == '"' || c.is_control() { '\'' } else { c })
        .collect()
}

fn run() -> Result<(), String> {
    let cfg = config::load()?;
    // `web_search` holds its token rather than the config: read once, like the
    // providers, and never re-read mid-session.
    web::set_brave_key(cfg.search.brave_key.clone());
    // Confinement is announced before anything runs, once, next to the
    // provider lines: weaker than promised must never be silent.
    if let Some(note) = sandbox::init() {
        eprintln!("nano: {}", strip_control(&note));
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
    // Named once, next to the splash: which instructions are in force is part
    // of the session's identity, not a warning.
    let notice = project_file
        .as_deref()
        .filter(|_| project_agents.is_some())
        .map(|f| format!("Using project instructions from {}", f.display()));

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
        banner(notice.as_deref());
    } else {
        // One leading blank: the same margin `ui::Screen` keeps on both edges.
        println!(
            " {}",
            terminal_text(&selection(&providers, provider, &model, &thinking))
        );
        if let Some(notice) = &notice {
            println!(" {}", terminal_text(notice));
        }
    }

    // Built once. The mode lives in the history, not here, so a toggle leaves
    // the prefix byte-identical and the provider's cache intact. The project's
    // file is tagged: it is data about the repository, not authority.
    let system = system_with_instructions(
        global_agents.as_deref(),
        project_agents.as_deref().zip(project_file.as_deref()),
        cfg.system.as_deref(),
    );
    // The mode is history, not prompt: the config decides where a session
    // starts, and the note below tells the model which side it is on.
    let read_only = cfg.explore_by_default;
    let messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        mode_note(read_only),
    ];

    // The directory's sessions: archive the old bodies before the listing so
    // the markers match what a chooser will resume.
    let mut startup = Vec::new();
    if let Some(dir) = &cwd {
        sessions::archive_old(dir, sessions::KEEP);
        startup = sessions::list(dir);
        startup.truncate(15);
    }
    let spec = format!("{}/{}", providers[provider].name, model);
    let store = cwd
        .as_deref()
        .map(|dir| Store::fresh(sessions::dir(dir), &sid, &spec));

    let session = Session {
        state,
        providers,
        provider,
        model,
        thinking,
        read_only,
        used: None,
        messages,
        sid,
        store,
        startup,
        picks: Vec::new(),
        naming: None,
        warning: None,
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
        .find(|text| !text.starts_with("explore mode:") && !text.starts_with('$'))?;
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

/// The note that tells the model which mode it is in. A `user` message, the
/// same role `!cmd` output joins as; the mode is history, so the prompt above
/// it never changes.
fn mode_note(read_only: bool) -> Value {
    let content = if read_only {
        "explore mode: on — write and edit are unavailable; bash runs read-only \
         commands, writes only to a scratch directory, and no command reaches the \
         network."
    } else {
        "explore mode: off — write, edit and a full bash are available; fetch and web_search \
         are unavailable; bash cannot reach the network."
    };
    json!({ "role": "user", "content": content })
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
    /// Explore mode: `read` and a read-only `bash` only (`/explore`). Starts
    /// from `cfg.explore_by_default`; the history carries the note.
    read_only: bool,
    /// The last round's token count, from the provider. `None` until a turn
    /// reports one: the fill is then estimated from the history.
    used: Option<u64>,
    messages: Vec<Value>,
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
}

/// The session on disk. `meta` is loaded with the first message or on resume,
/// so an idle launch leaves no file behind.
struct Store {
    dir: std::path::PathBuf,
    id: String,
    model: String,
    log: Option<sessions::Log>,
    meta: Option<sessions::Meta>,
    naming_tried: bool,
    warned: bool,
    warning: Option<String>,
}

impl Store {
    fn fresh(dir: std::path::PathBuf, id: &str, model: &str) -> Store {
        Store {
            dir,
            id: id.to_string(),
            model: model.to_string(),
            log: None,
            meta: None,
            naming_tried: false,
            warned: false,
            warning: None,
        }
    }

    fn resumed(
        dir: std::path::PathBuf,
        meta: sessions::Meta,
        model: &str,
    ) -> Result<Store, String> {
        let log = sessions::Log::open(&dir, &meta.id)?;
        Ok(Store {
            dir,
            id: meta.id.clone(),
            model: model.to_string(),
            log: Some(log),
            meta: Some(meta),
            naming_tried: false,
            warned: false,
            warning: None,
        })
    }

    /// A write failure is reported once and the session goes on: a session log
    /// must never fail a turn.
    fn record(&mut self, message: &Value) {
        if let Err(e) = self.try_record(message) {
            if !self.warned {
                self.warned = true;
                self.warn(format!("session: {e}"));
            }
        }
    }

    fn warn(&mut self, text: String) {
        if self.warning.is_none() {
            self.warning = Some(text);
        }
    }

    fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    fn try_record(&mut self, message: &Value) -> Result<(), String> {
        if self.log.is_none() {
            self.log = Some(sessions::Log::open(&self.dir, &self.id)?);
        }
        if self.meta.is_none() {
            let mut meta = sessions::Meta::new(&self.id, &self.model);
            if let Some(summary) = user_summary(message) {
                meta.summary = summary;
            }
            self.meta = Some(meta);
        }
        let meta = self.meta.as_mut().expect("just created");
        self.log.as_mut().expect("just opened").append(message)?;
        meta.messages += 1;
        meta.updated = sessions::now();
        if meta.summary.is_empty() {
            if let Some(summary) = user_summary(message) {
                meta.summary = summary;
            }
        }
        meta.save(&self.dir)
    }

    fn set_title(&mut self, title: &str, summary: &str) {
        let Some(meta) = self.meta.as_mut() else {
            return;
        };
        meta.title = sessions::collapse(title, 60);
        if !summary.trim().is_empty() {
            meta.summary = sessions::collapse(summary, 240);
        }
        if let Err(e) = meta.save(&self.dir) {
            self.warn(format!("session meta: {e}"));
        }
    }

    /// A title no model gave: the first request stands in. Called when the
    /// session ends without one.
    fn finalize(&mut self, messages: &[Value]) {
        let Some(meta) = self.meta.as_mut() else {
            return;
        };
        if meta.title.is_empty() {
            let (title, summary) = sessions::fallback(messages);
            if !title.is_empty() {
                meta.title = title;
                if meta.summary.is_empty() {
                    meta.summary = summary;
                }
            }
        }
        if let Err(e) = meta.save(&self.dir) {
            self.warn(format!("session meta: {e}"));
        }
    }
}

fn user_summary(message: &Value) -> Option<String> {
    if message["role"].as_str() != Some("user") {
        return None;
    }
    let text = message["content"].as_str()?;
    if text.starts_with("explore mode:") || text.starts_with('$') {
        return None;
    }
    Some(sessions::collapse(text, 240))
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
    Models,
    /// None opens the picker; Some is a `provider/id` or bare model id.
    Model(Option<String>),
    /// None prints the current level; Some sets it.
    Thinking(Option<String>),
    Thoughts,
    Explore,
    /// A chooser in the interactive run, a numbered list when piped.
    Sessions(Option<String>),
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
        "/models" => return Command::Models,
        "/model" => return Command::Model(None),
        "/thinking" => return Command::Thinking(None),
        "/thoughts" => return Command::Thoughts,
        "/explore" => return Command::Explore,
        "/sessions" => return Command::Sessions(None),
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
    "/exit",
    "/explore",
    "/help",
    "/model",
    "/models",
    "/new",
    "/quit",
    "/sessions",
    "/thinking",
    "/thoughts",
];

const HELP: &str = "/model [id]    pick or switch model\n\
                    /models        list models\n\
                    /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                    /thoughts      show or hide the model's reasoning\n\
                    /sessions [q]  list, search and resume sessions\n\
                    /new           start a new session\n\
                    /explore       toggle explore mode (read-only shell)\n\
                    !cmd           run a shell command; its output joins the context\n\
                    /exit          quit\n";

impl Session {
    /// One line at a time from a terminal we own. Keys arrive on the channel, so
    /// a turn in flight never blocks the prompt.
    fn run_interactive(mut self) -> Result<(), String> {
        let mut screen =
            ui::Screen::new("❯ ").map_err(|e| format!("cannot take the terminal: {e}"))?;
        screen.set_explore(self.read_only);
        screen.set_commands(COMMANDS);
        screen.set_info(&self.info());
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

        // A line submitted mid-turn waits here until the turn boundary, where it
        // is dispatched exactly like a line typed while idle. That is why slash
        // commands are handled in this loop and not only where they are parsed.
        let mut pending: Option<String> = None;
        loop {
            self.poll_naming(Some(&mut screen));
            let (line, was_queued) = match pending.take() {
                Some(line) => (line, true),
                None => match readline(&mut screen, &rx) {
                    Some(Input::Line(line)) => (line, false),
                    Some(Input::Explore) => {
                        self.toggle_explore(&mut screen);
                        continue;
                    }
                    Some(Input::Menu(index)) => {
                        let Some(meta) = self.startup.get(index).cloned() else {
                            continue;
                        };
                        screen.clear_menu(false);
                        self.startup.clear();
                        if let Err(e) = self.resume(&meta.id, Some(&mut screen)) {
                            screen.error(&format!("nano: {e}\n"));
                        }
                        continue;
                    }
                    Some(Input::Cancel) => {
                        self.startup.clear();
                        continue;
                    }
                    None => break,
                },
            };
            // A committed line is echoed by `readline`; a queued one was not.
            if was_queued {
                screen.echo_user(&line);
            }
            match self.dispatch(&line, &mut screen, &rx) {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    pending = self.messages_turn(&input, &tx, &rx, &mut screen)?;
                }
                Cmd::Shell(command) => {
                    pending = self.shell(&command, &mut screen, &rx);
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
                _ => dispatch_piped(&mut self, &line),
            };
            match cmd {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    let mut sink = PlainSink::default();
                    let cancel = http::Cancel::new();
                    self.record(json!({ "role": "user", "content": input }));
                    if let Err(e) = self.turn(&mut sink, &cancel) {
                        eprintln!("nano: {}", strip_control(&e));
                    }
                    self.name_session();
                }
                Cmd::Shell(command) => {
                    println!(
                        "{}",
                        terminal_text(&plain_text(&shell_line(&command, false)))
                    );
                    let result = self.run_shell(&command);
                    print!("{}", terminal_text(&result));
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
            Some(screen) => screen.error(&format!("nano: {text}\n")),
            None => eprintln!("nano: {}", strip_control(&text)),
        }
    }

    fn spec(&self) -> String {
        format!("{}/{}", self.providers[self.provider].name, self.model)
    }

    /// Switch model: reset the fill count, remember the level for the new
    /// `provider/id` (state first, then config), persist the selection, and
    /// return the new info row.
    fn set_model(&mut self, provider: usize, id: String) -> String {
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
        let model = self.spec();
        let store = Store::resumed(dir, meta, &model)?;

        let system = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| json!({ "role": "system", "content": tools::system_prompt() }));
        self.messages = vec![system, mode_note(self.read_only)];
        self.messages.extend(messages);
        self.sid = id.to_string();
        self.store = Some(store);
        self.naming = None;
        self.used = None;
        self.set_session_header();

        if let Some(screen) = screen {
            for warning in warnings {
                screen.error(&format!("nano: {warning}\n"));
            }
            screen.output(&format!("Resumed: {title}\n"));
            self.replay(screen);
            screen.set_info(&self.info());
        } else {
            for warning in warnings {
                eprintln!("nano: {}", strip_control(&warning));
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
                        let mut spans = call_line(&call, self.read_only);
                        spans.push((Paint::Plain, " ".to_string()));
                        let (paint, glyph) = mark(result);
                        spans.push((paint, glyph.to_string()));
                        screen.insert(vec![line_of(&spans)], ui::Block::Tool);
                        if failed(result) {
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
        let model = self.spec();
        let dir = self.store.as_ref().map(|store| store.dir.clone());
        if let Some(dir) = dir {
            self.store = Some(Store::fresh(dir, &sid, &model));
        }
        let system = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| json!({ "role": "system", "content": tools::system_prompt() }));
        self.messages = vec![system, mode_note(self.read_only)];
        self.sid = sid;
        self.naming = None;
        self.picks.clear();
        self.used = None;
        self.set_session_header();
        if let Some(screen) = screen {
            screen.clear_menu(false);
            screen.output("New session\n");
            screen.set_info(&self.info());
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
    fn poll_naming(&mut self, screen: Option<&mut ui::Screen>) {
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
            match readline(screen, rx) {
                Some(Input::Menu(index)) => {
                    let Some(id) = ids.get(index).cloned() else {
                        continue;
                    };
                    screen.clear_menu(false);
                    match self.resume(&id, Some(screen)) {
                        Ok(_) => return,
                        Err(e) => {
                            screen.error(&format!("nano: {e}\n"));
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

    /// The no-tty form: a numbered list, and a number to resume. A search
    /// replaces the list, so the numbers always refer to what was last shown.
    fn list_piped(&mut self, arg: &str) {
        if self.state.cwd.as_os_str().is_empty() {
            eprintln!("nano: sessions need a working directory");
            return;
        }
        if let Ok(number) = arg.parse::<usize>() {
            let Some(id) = number
                .checked_sub(1)
                .and_then(|at| self.picks.get(at))
                .cloned()
            else {
                eprintln!("nano: no session #{number}; run /sessions first");
                return;
            };
            match self.resume(&id, None) {
                Ok(title) => println!("Resumed: {}", terminal_text(&title)),
                Err(e) => eprintln!("nano: {}", strip_control(&e)),
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
    fn info(&self) -> String {
        let base = selection(&self.providers, self.provider, &self.model, &self.thinking);
        let mut text = format!("{base}  ·  {}", self.fill());
        if self.read_only {
            text.push_str("  ·  read-only");
        }
        text
    }

    /// Used and total tokens, with the percentage between them. The provider's
    /// own count when the last turn reported one, a character estimate
    /// otherwise; no percentage when the model does not say how big its window
    /// is.
    fn fill(&self) -> String {
        let used = self.used.unwrap_or_else(|| estimate(&self.messages));
        match self.context() {
            Some(ctx) => format!("{}/{} ({})", scaled(used), scaled(ctx), percent(used, ctx)),
            None => scaled(used),
        }
    }

    fn context(&self) -> Option<u64> {
        let models = &self.providers[self.provider].models;
        models.iter().find(|m| m.id == self.model)?.context
    }

    /// Enter or leave explore mode. The prompt and the tool catalogue are
    /// fixed, so the mode is announced in the history: the model reads the
    /// note, and nothing before it is rewritten.
    fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        self.messages.push(mode_note(read_only));
    }

    /// `/explore` and Shift+Tab share one path, so the prompt color, the
    /// selection row and the transcript never disagree about the mode.
    fn toggle_explore(&mut self, screen: &mut ui::Screen) {
        self.set_read_only(!self.read_only);
        screen.set_explore(self.read_only);
        screen.set_info(&self.info());
        screen.output(&format!(
            "Explore: {}\n",
            if self.read_only { "on" } else { "off" }
        ));
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
        let _ = screen.draw();
        let cancel = http::Cancel::new();
        let args = json!({ "command": command }).to_string();
        let mut queued: Option<String> = None;
        let result = thread::scope(|scope| {
            let handle = scope.spawn(|| tools::execute("bash", &args, &cancel, false));
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Event::Key(key)) if key.code == KeyCode::Esc => {
                        cancel.cancel();
                        screen.set_status("Interrupting…");
                        let _ = screen.draw();
                    }
                    Ok(Event::Key(key)) => match screen.apply_key(key) {
                        ui::Action::Submit if !screen.is_empty() && queued.is_none() => {
                            queued = Some(screen.take());
                        }
                        ui::Action::Eof => {
                            cancel.cancel();
                            break;
                        }
                        _ => {
                            let _ = screen.draw();
                        }
                    },
                    Ok(Event::Paste(text)) => {
                        screen.insert_text(&text);
                        let _ = screen.draw();
                    }
                    Ok(Event::Resize) => {
                        let _ = screen.draw();
                    }
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
            }
            handle
                .join()
                .unwrap_or_else(|_| "error: shell thread panicked".into())
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
        screen.set_info(&self.info());
        let _ = screen.draw();
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
            false,
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

        let cancel = http::Cancel::new();
        let mut queued: Option<String> = None;
        // Streamed text up to the last newline is permanent; the rest is the
        // line still being written and lives in the status row.
        let mut partial = String::new();
        // Reasoning before the answer. Its last line is unfinished until the
        // answer starts or the round ends, so it is held until then.
        let mut thought = String::new();

        screen.set_status("Thinking…");
        let _ = screen.draw();

        // What the last round counted, handed back by the turn's thread. The
        // history is borrowed by that thread, so `self` is out of reach until
        // the scope ends.
        let mut used: Option<u64> = None;
        let messages = &mut self.messages;
        let store = self.store.as_mut();
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        let thoughts = self.state.thoughts;
        let read_only = self.read_only;
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
                let result = run_turn(
                    provider,
                    &model,
                    messages,
                    store,
                    thinking.as_deref(),
                    read_only,
                    &mut sink,
                    turn_cancel,
                );
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
                let mut dirty = false;
                let mut done = false;
                let Some(first) = first else {
                    continue;
                };
                let mut next = Some(first);
                while let Some(ev) = next.take().or_else(|| rx.try_recv().ok()) {
                    match ev {
                        // ESC is the interrupt, and it means one thing: stop
                        // the turn now. The flag ends it between requests; the
                        // socket shutdown unblocks the one in flight.
                        Event::Key(key) if key.code == KeyCode::Esc => cancel.cancel(),
                        Event::Key(key) => match screen.apply_key(key) {
                            ui::Action::Eof => {
                                cancel.cancel();
                                done = true;
                            }
                            ui::Action::Submit => {
                                if !screen.is_empty() && queued.is_none() {
                                    // Queue without echoing: the line is
                                    // dispatched at the boundary, below the
                                    // answer it arrived during.
                                    queued = Some(screen.take());
                                    screen.set_queued(queued.as_deref());
                                    dirty = true;
                                }
                            }
                            // The turn already captured its tool set; a mode
                            // switch takes effect on the next one.
                            ui::Action::Explore => {}
                            // A chooser only exists while idle.
                            ui::Action::MenuStep(_)
                            | ui::Action::MenuAccept
                            | ui::Action::MenuCancel => {}
                            ui::Action::None => dirty = true,
                        },
                        Event::Paste(text) => {
                            screen.insert_text(&text);
                            dirty = true;
                        }
                        // Model text, already spaced by the sink: everything up
                        // to the last newline is a finished line.
                        Event::Printed(text) => {
                            dirty = true;
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
                            dirty = true;
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
                            dirty = true;
                            thought.push_str(&text);
                            let finished = match thought.rfind('\n') {
                                Some(at) => thought.drain(..=at).collect::<String>(),
                                None => String::new(),
                            };
                            screen.thought(&finished);
                        }
                        Event::Status(text) => {
                            if screen.status() != text {
                                dirty = true;
                                screen.set_status(&text);
                            }
                        }
                        Event::Resize => dirty = true,
                        Event::Done(Err(e)) => {
                            screen.error(&format!("nano: {e}\n"));
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
                if dirty {
                    let _ = screen.draw();
                }
                if done {
                    break;
                }
            }
            handle.join().is_ok()
        });
        screen.set_status("");
        screen.set_queued(None);
        // A turn that reported nothing (interrupted, or a provider that does
        // not count) leaves the estimate in charge rather than a stale number.
        self.used = used;
        screen.set_info(&self.info());

        // One small request names the session, once, off the UI thread. The
        // prompt is live immediately; the result is applied at the next poll.
        self.start_naming();
        self.flush_warnings(Some(screen));

        if !result {
            screen.error("nano: turn failed\n");
        }
        Ok(queued)
    }

    fn turn(&mut self, sink: &mut dyn Sink, cancel: &http::Cancel) -> Result<(), String> {
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        run_turn(
            provider,
            &model,
            &mut self.messages,
            self.store.as_mut(),
            thinking.as_deref(),
            self.read_only,
            sink,
            cancel,
        )
        .map(|_| ())
    }

    /// Slash commands and selection changes. `screen` supplies a line from the
    /// terminal for the model picker.
    fn dispatch(&mut self, line: &str, screen: &mut ui::Screen, rx: &Receiver<Event>) -> Cmd {
        match parse_command(line) {
            Command::Empty => Cmd::Continue,
            Command::Exit => Cmd::Exit,
            Command::Help => {
                screen.output(HELP);
                Cmd::Continue
            }
            Command::New => {
                self.new_session(Some(screen));
                Cmd::Continue
            }
            Command::Models => {
                screen.output(&models(&self.providers));
                Cmd::Continue
            }
            Command::Model(None) => {
                if let Some((pi, id)) =
                    pick(&self.providers, (self.provider, &self.model), screen, rx)
                {
                    let text = self.set_model(pi, id);
                    screen.set_info(&text);
                    screen.output(&format!("{text}\n"));
                }
                Cmd::Continue
            }
            Command::Model(Some(spec)) => {
                match find(&self.providers, &spec) {
                    Some((pi, id)) => {
                        let text = self.set_model(pi, id);
                        screen.set_info(&text);
                        screen.output(&format!("{text}\n"));
                    }
                    None => screen.error(&format!("unknown model: {spec}\n")),
                }
                Cmd::Continue
            }
            Command::Thinking(rest) => {
                let providers = &self.providers;
                if rest.as_deref().unwrap_or("").is_empty() {
                    screen.output(&format!(
                        "thinking: {}\n\n",
                        thinking_label(&providers[self.provider].cfg, &self.thinking)
                    ));
                    return Cmd::Continue;
                }
                match set_thinking(&providers[self.provider].cfg, rest.as_deref().unwrap_or("")) {
                    Ok(level) => {
                        self.thinking = Some(level.clone());
                        screen.set_info(&self.info());
                        let spec = format!("{}/{}", providers[self.provider].name, self.model);
                        self.state.set_thinking(&spec, level.as_str());
                        let save_error = self.state.save().err();
                        let label = thinking_label(&providers[self.provider].cfg, &self.thinking);
                        if let Some(e) = save_error {
                            self.warn(format!("state: {e}"));
                        }
                        screen.output(&format!("thinking: {label}\n\n"));
                    }
                    Err(e) => screen.error(&format!("nano: {e}\n")),
                }
                Cmd::Continue
            }
            Command::Thoughts => {
                self.state.thoughts = !self.state.thoughts;
                if let Err(e) = self.state.save() {
                    self.warn(format!("state: {e}"));
                }
                screen.output(&format!(
                    "Thoughts: {}\n\n",
                    if self.state.thoughts { "on" } else { "off" }
                ));
                Cmd::Continue
            }
            Command::Explore => {
                self.toggle_explore(screen);
                Cmd::Continue
            }
            Command::Sessions(query) => {
                self.browse_sessions(screen, rx, query.as_deref().unwrap_or(""));
                Cmd::Continue
            }
            Command::Unknown(input) => {
                screen.error(&format!("unknown command: {input}\n"));
                Cmd::Continue
            }
            Command::Shell(command) => Cmd::Shell(command),
            Command::Turn(input) => Cmd::Turn(input),
        }
    }
}

/// The same dispatch for a terminal that is not ours: commands only, warnings
/// on stderr, no picker.
fn dispatch_piped(session: &mut Session, line: &str) -> Cmd {
    match parse_command(line) {
        Command::Empty => Cmd::Continue,
        Command::Exit => Cmd::Exit,
        Command::Help => {
            println!("{}", HELP.trim_end());
            Cmd::Continue
        }
        Command::New => {
            session.new_session(None);
            println!("New session");
            Cmd::Continue
        }
        Command::Models => {
            println!("{}", terminal_text(models(&session.providers).trim_end()));
            Cmd::Continue
        }
        Command::Model(None) => {
            eprintln!("nano: /model needs a terminal");
            Cmd::Continue
        }
        Command::Model(Some(spec)) => {
            match find(&session.providers, &spec) {
                Some((pi, id)) => println!("{}", terminal_text(&session.set_model(pi, id))),
                None => eprintln!("nano: unknown model: {}", strip_control(&spec)),
            }
            Cmd::Continue
        }
        Command::Thinking(rest) => {
            let providers = &session.providers;
            if rest.as_deref().unwrap_or("").is_empty() {
                println!(
                    "thinking: {}",
                    terminal_text(&thinking_label(
                        &providers[session.provider].cfg,
                        &session.thinking
                    ))
                );
                return Cmd::Continue;
            }
            match set_thinking(
                &providers[session.provider].cfg,
                rest.as_deref().unwrap_or(""),
            ) {
                Ok(level) => {
                    session.thinking = Some(level.clone());
                    let spec = format!("{}/{}", providers[session.provider].name, session.model);
                    session.state.set_thinking(&spec, level.as_str());
                    if let Err(e) = session.state.save() {
                        eprintln!("nano: {}", strip_control(&e));
                    }
                }
                Err(e) => eprintln!("nano: {}", strip_control(&e)),
            }
            Cmd::Continue
        }
        Command::Thoughts => {
            eprintln!("nano: /thoughts needs a terminal");
            Cmd::Continue
        }
        Command::Explore => {
            session.set_read_only(!session.read_only);
            println!("Explore: {}", if session.read_only { "on" } else { "off" });
            Cmd::Continue
        }
        Command::Sessions(query) => {
            session.list_piped(query.as_deref().unwrap_or(""));
            Cmd::Continue
        }
        Command::Unknown(input) => {
            eprintln!("nano: unknown command: {}", strip_control(&input));
            Cmd::Continue
        }
        Command::Shell(command) => Cmd::Shell(command),
        Command::Turn(input) => Cmd::Turn(input),
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
    /// The terminal changed size; the live region must repaint.
    Resize,
    Done(Result<Option<u64>, String>),
    Eof,
}

/// The terminal's keys. Nothing else may read stdin.
fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        use ratatui::crossterm::event::{poll, read, Event as TermEvent};
        loop {
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
                    if tx.send(Event::Key(key)).is_err() {
                        return;
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

/// Collect one line from the prompt. Keys are the only input while idle.
fn readline(screen: &mut ui::Screen, rx: &Receiver<Event>) -> Option<Input> {
    let _ = screen.draw();
    loop {
        match rx.recv() {
            Ok(Event::Key(key)) => match screen.apply_key(key) {
                ui::Action::Submit if screen.is_empty() => {}
                ui::Action::Submit => return Some(Input::Line(screen.commit())),
                ui::Action::Explore => return Some(Input::Explore),
                ui::Action::MenuStep(delta) => {
                    screen.menu_move(delta);
                    let _ = screen.draw();
                }
                ui::Action::MenuAccept => return Some(Input::Menu(screen.menu_selected())),
                ui::Action::MenuCancel => {
                    // The list stays in the transcript: a chooser that closes
                    // must not leave blank rows where it was.
                    screen.clear_menu(true);
                    let _ = screen.draw();
                    return Some(Input::Cancel);
                }
                ui::Action::Eof => return None,
                ui::Action::None => {
                    let _ = screen.draw();
                }
            },
            Ok(Event::Paste(text)) => {
                screen.insert_text(&text);
                let _ = screen.draw();
            }
            Ok(Event::Resize) => {
                let _ = screen.draw();
            }
            Ok(Event::Eof) | Err(_) => return None,
            // A turn's events cannot arrive while idle; drop them if they do.
            Ok(_) => {}
        }
    }
}

/// Where a turn's output goes: straight to stdout when piped, onto the channel
/// when a prompt is on screen.
trait Sink {
    /// Streamed model text. `TextSink` owns the spacing: finished lines go out
    /// permanently, the unfinished one is exposed as a tail.
    fn text(&mut self, text: &str);
    /// Streamed reasoning. Shown or not is the sink's business; when it is
    /// hidden, one line with its duration stands in for it.
    fn reasoning(&mut self, text: &str);
    /// A transient label for the live region: "Thinking…", a running command.
    fn status(&mut self, text: &str);
    /// One tool call, finished: the call and its outcome on one line, and the
    /// failure's own words beneath it.
    fn echo(&mut self, call: &[(Paint, String)], mark: (Paint, &'static str), error: Option<&str>);
    /// A round's text is over; close its last line if it wrote one.
    fn end_round(&mut self);
}

#[derive(Default)]
struct PlainSink {
    out: TextSink,
    md: markdown::Stream,
    /// The line being written: the stream ends wherever the provider felt
    /// like it, and a line is only a line once its newline arrives.
    partial: String,
    /// Whether a block has already been written: the renderer decides where
    /// blocks end, so the blank line between them is put here.
    wrote_block: bool,
}

impl PlainSink {
    fn write(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        let mut buf = String::new();
        if self.wrote_block {
            buf.push('\n');
        }
        self.wrote_block = true;
        for line in &lines {
            buf.push_str(&markdown::ansi(line));
            buf.push('\n');
        }
        let mut done = String::new();
        self.out.push(&buf, &mut done);
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
        self.write(lines);
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
            self.write(lines);
        }
    }

    fn status(&mut self, _text: &str) {}

    fn echo(&mut self, call: &[(Paint, String)], mark: (Paint, &'static str), error: Option<&str>) {
        self.flush();
        let mut line = String::new();
        for (paint, text) in call {
            line.push_str(&paint.ansi(text));
        }
        line.push(' ');
        line.push_str(&mark.0.ansi(mark.1));
        println!("{line}");
        if let Some(error) = error {
            println!("    {}", Paint::Red.ansi(error));
        }
        io::stdout().flush().ok();
    }

    fn end_round(&mut self) {
        self.flush();
        self.out.commit();
        if std::mem::take(&mut self.out.wrote) {
            println!();
        }
        self.wrote_block = false;
    }

    fn reasoning(&mut self, _text: &str) {}
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
        if std::mem::take(&mut self.out.wrote) {
            // The reply ended without a final newline, so the line it left
            // unfinished becomes permanent here.
            self.send(Event::Printed("\n".to_string()));
        }
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

/// Drop control characters a terminal would execute. Newlines and tabs stay:
/// this is line output, not a raw terminal stream.
fn strip_control(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Text bound for stdout: stripped only when a terminal would interpret it,
/// so redirected output stays byte-exact.
fn terminal_text(text: &str) -> String {
    if io::stdout().is_terminal() {
        strip_control(text)
    } else {
        text.to_string()
    }
}

/// A style this binary uses for transcript text. Two renderers read it: the
/// screen's cells, and ANSI escapes when there is no screen.
#[derive(Clone, Copy)]
enum Paint {
    Plain,
    Dim,
    Cyan,
    Yellow,
    Green,
    Red,
}

impl Paint {
    fn style(self) -> Style {
        match self {
            Paint::Plain => Style::new(),
            Paint::Dim => Style::new().add_modifier(Modifier::DIM),
            Paint::Cyan => Style::new().fg(Color::Cyan),
            Paint::Yellow => Style::new().fg(Color::Yellow),
            Paint::Green => Style::new().fg(Color::Green),
            Paint::Red => Style::new().fg(Color::Red),
        }
    }

    /// ANSI colors only when stdout is a terminal, so redirected output stays
    /// plain. A terminal also gets its control bytes stripped: content can come
    /// from a fetched page or a command, and ESC is never part of the style.
    fn ansi(self, text: &str) -> String {
        if !io::stdout().is_terminal() {
            return text.to_string();
        }
        let text = strip_control(text);
        let code = match self {
            Paint::Plain => return text,
            Paint::Dim => "2",
            Paint::Cyan => "36",
            Paint::Yellow => "33",
            Paint::Green => "32",
            Paint::Red => "31",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    fn span(self, text: String) -> Span<'static> {
        Span::styled(text, self.style())
    }
}

fn line_of(spans: &[(Paint, String)]) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|(paint, text)| paint.span(text.clone()))
            .collect::<Vec<_>>(),
    )
}

fn plain_text(spans: &[(Paint, String)]) -> String {
    spans
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<String>()
        .trim()
        .to_string()
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
            "nano: {}: thinking level \"{level}\" is not configured, using default",
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
    let level = match thinking {
        Some(level) => level.clone(),
        None => "default".to_string(),
    };
    format!("{}/{}  ·  {}", providers[provider].name, model, level)
}

/// Four bytes per token, the usual rule of thumb for English. Only used when no
/// provider has counted the real thing: it is off by tens of percent.
fn estimate(messages: &[Value]) -> u64 {
    messages
        .iter()
        .map(|m| m.to_string().len() as u64)
        .sum::<u64>()
        / 4
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

/// Splash, printed once above the live region: block letters for `nano`. It
/// lands in the transcript and is never repainted, so it must be narrower than
/// the terminal's margin-to-margin width on a narrow window.
const LOGO: &[&str] = &[
    "████▄  ▀▀█▄ ████▄ ▄███▄",
    "██ ██ ▄█▀██ ██ ██ ██ ██",
    "██ ██ ▀█▄██ ██ ██ ▀███▀",
];

fn banner(note: Option<&str>) {
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
        &["nano"]
    };
    for line in logo {
        println!(" {}", Paint::Cyan.ansi(line));
    }
    if let Some(note) = note {
        println!();
        println!(" {}", Paint::Dim.ansi(note));
    }
    println!();
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
        eprintln!("nano: model {spec:?} not available, using default");
    }
    (0, providers[0].models[0].id.clone())
}

fn pick(
    providers: &[Provider],
    current: (usize, &str),
    screen: &mut ui::Screen,
    rx: &Receiver<Event>,
) -> Option<(usize, String)> {
    let mut items: Vec<(usize, String)> = Vec::new();
    let mut text = String::new();
    for (pi, p) in providers.iter().enumerate() {
        for m in &p.models {
            let mark = if pi == current.0 && m.id == current.1 {
                "*"
            } else {
                " "
            };
            text.push_str(&format!(
                "{mark}{:>2}) {}/{}\n",
                items.len() + 1,
                p.name,
                m.id
            ));
            items.push((pi, m.id.clone()));
        }
    }

    screen.output(&text);
    let saved = screen.prompt().to_string();
    screen.set_prompt("model> ");
    // A mode toggle is not a model choice: keep the picker open.
    let choice = loop {
        match readline(screen, rx) {
            Some(Input::Line(line)) => break line,
            Some(Input::Explore) => continue,
            // There is no chooser here; a menu key cannot arrive.
            Some(Input::Menu(_)) | Some(Input::Cancel) => continue,
            None => break String::new(),
        }
    };
    screen.set_prompt(&saved);
    let choice = choice.trim().to_string();
    if choice.is_empty() {
        return None;
    }

    if let Ok(n) = choice.parse::<usize>() {
        return match items.get(n.wrapping_sub(1)) {
            Some(hit) if n >= 1 => Some(hit.clone()),
            _ => {
                screen.error(&format!("no model #{n}\n"));
                None
            }
        };
    }

    match find(providers, &choice) {
        Some(hit) => Some(hit),
        None => {
            screen.error(&format!("unknown model: {choice}\n"));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_turn(
    provider: &Provider,
    model: &str,
    messages: &mut Vec<Value>,
    mut store: Option<&mut Store>,
    thinking: Option<&str>,
    read_only: bool,
    sink: &mut dyn Sink,
    cancel: &http::Cancel,
) -> Result<Option<u64>, String> {
    let tools = tools::definitions();
    // The last round's own count. It measures the whole request, so it is the
    // fill; `None` when no round reported one.
    let mut used = None;

    loop {
        // An interrupt ends the turn here too: no further round is started.
        // The user message stays without a reply, which is what happened.
        if cancel.cancelled() {
            return Ok(used);
        }
        let extra = provider.cfg.thinking_body(thinking);
        // Both callbacks borrow the sink, so it is shared rather than captured
        // twice. The borrow ends with the block, so the turn can still report
        // through it after the stream is over.
        let result = {
            let sink = RefCell::new(&mut *sink);
            api::stream_chat(
                &provider.ep,
                provider.key.as_deref(),
                &provider.headers,
                model,
                messages,
                tools,
                extra.as_ref(),
                cancel,
                |text| sink.borrow_mut().text(text),
                |text| sink.borrow_mut().reasoning(text),
            )
        };
        let reply = match result {
            Ok(reply) => reply,
            // Closing the socket under a read is how the interrupt unblocks
            // it; the error is the interruption, not a failure. What arrived
            // is still printed, but the turn is over.
            Err(_) if cancel.cancelled() => {
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
        if cancel.cancelled() {
            return Ok(used);
        }

        if reply.calls.is_empty() {
            let message = assistant_message(&reply);
            if let Some(store) = store.as_deref_mut() {
                store.record(&message);
            }
            messages.push(message);
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
        if let Some(store) = store.as_deref_mut() {
            store.record(&message);
        }
        messages.push(message);

        for call in &reply.calls {
            if cancel.cancelled() {
                return Ok(used);
            }
            let echo = call_line(call, read_only);
            // What is running, while it runs: the finished line appears below
            // once there is a result to put on it.
            sink.status(&plain_text(&echo));
            let result = tools::execute(&call.name, &call.arguments, cancel, read_only);
            let error = failed(&result).then(|| detail(&result));
            sink.echo(&echo, mark(&result), error.as_deref());
            let message = json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result,
            });
            if let Some(store) = store.as_deref_mut() {
                store.record(&message);
            }
            messages.push(message);
            if cancel.cancelled() {
                return Ok(used);
            }
        }
    }
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
struct TextSink {
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
    fn push(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            if ch == '\n' {
                if self.started {
                    self.pending += 1;
                }
                continue;
            }
            if !self.started {
                self.started = true;
                // One blank line separates a reply from whatever precedes it.
                // Emitted here, not per round: a round with no text must not
                // widen the gap between two tool calls.
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

    /// End the reply: blank lines held back at its tail are dropped, since the
    /// harness owns the spacing between blocks.
    fn commit(&mut self) {
        self.pending = 0;
    }
}

// A shell command, the user's (`!cmd`) or the model's, on one line: the glyph
// says what runs, and the command follows it, since `bash` has no call syntax.
fn shell_line(command: &str, read_only: bool) -> Vec<(Paint, String)> {
    // Cyan for a command that cannot change the tree: dim is the thought's
    // color, and the two must not read alike.
    let paint = if read_only {
        Paint::Cyan
    } else {
        Paint::Yellow
    };
    vec![
        (Paint::Plain, "  ".to_string()),
        (paint, "▷".to_string()),
        (Paint::Plain, " ".to_string()),
        (paint, summarize(command, 120)),
    ]
}

// Echo tool calls compactly: show the arguments that identify the target, never
// the raw JSON (heredoc scripts and long content are unreadable in a line).
// A glyph per tool says what it does at a glance. The palette has one rule:
// anything that can change the tree is yellow, anything that cannot is cyan
// (dim is the thought's color, so a read-only call must not use it).
fn call_line(call: &api::ToolCall, read_only: bool) -> Vec<(Paint, String)> {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let (glyph, paint) = match call.name.as_str() {
        "read" => ("◇", Paint::Cyan),
        "fetch" => ("↗", Paint::Cyan),
        "web_search" => ("⌕", Paint::Cyan),
        "about" => ("ℹ", Paint::Cyan),
        "write" => ("✚", Paint::Yellow),
        "edit" => ("✎", Paint::Yellow),
        _ => ("✚", Paint::Yellow),
    };

    if call.name == "bash" {
        let command = v["command"].as_str().unwrap_or("bash");
        return shell_line(command, read_only);
    }

    let mut parts = Vec::new();
    for key in ["path", "file", "pattern", "url", "query"] {
        if let Some(s) = v[key].as_str() {
            parts.push(summarize(s, 120));
        }
    }
    // A partial read is only reproducible with its window, so echo it. `fetch`
    // pages with start/chars; `read` with offset/limit.
    let num = |key: &str| v[key].as_u64().map(|n| n.to_string());
    let (offset, limit) = (
        num("offset").or_else(|| num("start")),
        num("limit").or_else(|| num("chars")),
    );
    if offset.is_some() || limit.is_some() {
        let start = offset.unwrap_or_else(|| "1".into());
        parts.push(match limit {
            Some(l) => format!("{start}+{l}"),
            None => format!("{start}+"),
        });
    }

    let mut spans = vec![
        (Paint::Plain, "  ".to_string()),
        (paint, glyph.to_string()),
        (Paint::Plain, " ".to_string()),
        (paint, call.name.clone()),
    ];
    if parts.is_empty() {
        return spans;
    }
    spans.push((Paint::Plain, format!("({})", parts.join(" "))));
    spans
}

/// A failed tool result is a result, not an error: tools report failures as
/// text, either `error: ...` or a non-zero `exit: N`.
fn failed(result: &str) -> bool {
    if let Some(rest) = result.strip_prefix("exit: ") {
        rest.lines().next().unwrap_or("").trim() != "0"
    } else {
        result.starts_with("error:")
    }
}

/// One glyph for the outcome.
fn mark(result: &str) -> (Paint, &'static str) {
    if failed(result) {
        (Paint::Red, "✗")
    } else {
        (Paint::Green, "✓")
    }
}

/// What to say under a failed call: one line. The `exit: N` header is dropped,
/// since the mark already reports the failure. The full result still reaches
/// the model; a build log must not reach the transcript.
fn detail(result: &str) -> String {
    let body = match result.strip_prefix("exit: ") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body).unwrap_or(""),
        None => result,
    };
    summarize(body.trim_start_matches('\n'), 120)
}

fn summarize(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.lines().count() > 1 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_bytes_are_stripped_but_lines_survive() {
        assert_eq!(strip_control("a\x1b[31mb\nc\td\u{9b}"), "a[31mb\nc\td");
    }

    #[test]
    fn project_instructions_are_tagged_between_global_and_system() {
        let system = system_with_instructions(
            Some("global text"),
            Some(("project text", Path::new("/repo/AGENTS.md"))),
            Some("config text"),
        );
        assert!(system.contains("# Instructions (global)\n\nglobal text"));
        assert!(system.contains(
            "<project_instructions path=\"/repo/AGENTS.md\">\nproject text\n</project_instructions>"
        ));
        assert!(
            system.find("# Instructions (global)").unwrap()
                < system.find("<project_instructions path=").unwrap()
        );
        assert!(
            system.find("<project_instructions path=").unwrap()
                < system.find("config text").unwrap()
        );
        assert_eq!(tag_path("a\"b\nc"), "a'b'c");
    }

    #[test]
    fn parses_commands_once() {
        assert_eq!(parse_command("  "), Command::Empty);
        assert_eq!(parse_command("/model"), Command::Model(None));
        assert_eq!(
            parse_command("/model a/b"),
            Command::Model(Some("a/b".into()))
        );
        assert_eq!(parse_command("/models"), Command::Models);
        assert_eq!(parse_command("/thinking"), Command::Thinking(None));
        assert_eq!(
            parse_command("/thinking high"),
            Command::Thinking(Some("high".into()))
        );
        assert_eq!(
            parse_command("/sessions q"),
            Command::Sessions(Some("q".into()))
        );
        assert_eq!(parse_command("! ls"), Command::Shell("ls".into()));
        assert_eq!(parse_command("!"), Command::Empty);
        assert_eq!(parse_command("hello"), Command::Turn("hello".into()));
        assert_eq!(parse_command("/modelx"), Command::Unknown("/modelx".into()));
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
    fn mode_note_names_the_available_tools() {
        let on = mode_note(true)["content"].as_str().unwrap().to_string();
        let off = mode_note(false)["content"].as_str().unwrap().to_string();
        assert!(on.contains("write"));
        assert!(off.contains("fetch and web_search are unavailable"));
    }

    #[test]
    fn display_helpers() {
        assert_eq!(scaled(9999), "9999");
        assert_eq!(scaled(10_000), "10k");
        assert_eq!(scaled(1_000_000), "1.0M");
        assert_eq!(percent(1, 1000), "<1%");
        assert_eq!(percent(50, 100), "50%");
        assert!(failed("exit: 1\nboom"));
        assert!(!failed("exit: 0\nok"));
        assert!(failed("error: nope"));
        assert_eq!(mark("exit: 0\n").1, "✓");
        assert_eq!(mark("exit: 1\n").1, "✗");
        assert_eq!(detail("exit: 1\nboom\nmore"), "boom…");
        assert_eq!(summarize("a\nb", 10), "a…");
    }

    #[test]
    fn session_ids_are_32_hex_chars() {
        let id = session_id();
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
                api_key: None,
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
                api_key: None,
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
            read_only: true,
            used: Some(10),
            messages: vec![json!({ "role": "system", "content": "s" })],
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: vec!["old".into()],
            naming: None,
            warning: None,
        };
        let info = session.set_model(0, "m".into());
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
        assert_eq!(
            user_summary(&json!({ "role": "user", "content": "do it" })),
            Some("do it".into())
        );
        assert!(user_summary(&json!({ "role": "user", "content": "$ ls" })).is_none());
        let meta = sessions::Meta {
            id: "abc123".into(),
            created: 0,
            updated: 0,
            title: "Title".into(),
            summary: String::new(),
            model: "p/m".into(),
            messages: 3,
            archived: true,
        };
        assert_eq!(session_title(&meta), "Title");
        assert!(session_row(&meta).contains("Title"));
        assert!(session_row(&meta).contains("zip"));
    }

    #[test]
    fn store_records_and_finalizes() {
        let dir = std::env::temp_dir().join(format!("nano-test-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::fresh(dir.clone(), "abc123", "p/m");
        store.record(&json!({ "role": "user", "content": "fix it" }));
        assert!(store.take_warning().is_none());
        assert_eq!(store.meta.as_ref().unwrap().messages, 1);
        store.set_title("T", "S");
        assert_eq!(store.meta.as_ref().unwrap().title, "T");
        store.finalize(&[]);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn session_info_and_mode_note() {
        let mut session = Session {
            state: state::State::default(),
            providers: vec![test_provider()],
            provider: 0,
            model: "m".into(),
            thinking: None,
            read_only: true,
            used: None,
            messages: vec![json!({ "role": "system", "content": "s" })],
            sid: "abc".into(),
            store: None,
            startup: Vec::new(),
            picks: Vec::new(),
            naming: None,
            warning: None,
        };
        assert!(session.info().contains("p/m"));
        assert_eq!(session.context(), Some(100));
        assert!(session.fill().contains("100"));
        session.set_read_only(false);
        assert!(!session.read_only);
        assert_eq!(session.messages.len(), 2);
        session.record(json!({ "role": "user", "content": "hi" }));
        assert_eq!(session.messages.len(), 3);
        session.warn("careful".into());
        session.flush_warnings(None);
    }
}
