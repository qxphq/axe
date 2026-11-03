mod api;
mod config;
mod http;
mod md;
mod ui;
mod tools;

use std::cell::RefCell;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEventKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{json, Value};

struct Provider {
    name: String,
    cfg: config::ProviderConfig,
    ep: http::Endpoint,
    key: Option<String>,
    headers: Vec<(String, String)>,
    models: Vec<api::ModelInfo>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("nano: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cfg = config::load()?;
    // Instructions are read once at startup, before any request: the system
    // prompt is a stable prefix and must not change mid-session.
    // Order: config `agents`, then the project's AGENTS.md, then `system`.
    let global_agents = cfg.agents.as_deref().and_then(config::read_agents);
    let project_file = std::env::current_dir()
        .ok()
        .and_then(|dir| config::find_agents(&dir))
        .filter(|f| Some(f.as_path()) != cfg.agents.as_deref());
    let project_agents = project_file.as_deref().and_then(config::read_agents);
    // Named once, next to the splash: which instructions are in force is part
    // of the session's identity, not a warning.
    let notice = project_file
        .as_deref()
        .filter(|_| project_agents.is_some())
        .map(|f| format!("Using project instructions from {}", f.display()));

    let mut providers = Vec::new();
    let session = session_id();

    for pc in &cfg.providers {
        let ep = http::parse_url(&pc.base_url).map_err(|e| format!("{}: {e}", pc.name))?;
        let key = pc.resolved_key();
        if key.is_none() {
            if let Some(var) = &pc.api_key_env {
                eprintln!("{}: {var} is not set, skipping", pc.name);
                continue;
            }
        }

        let mut headers = Vec::new();
        if let Some(name) = &pc.session_header {
            headers.push((name.clone(), session.clone()));
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
            Ok(_) => eprintln!("{}: no models", pc.name),
            Err(e) => eprintln!("{}: {e}", pc.name),
        }
    }

    if providers.is_empty() {
        return Err("no models available".into());
    }

    let (provider, model) = resolve(&providers, cfg.model.as_deref());
    // Thinking level follows the model: it is remembered per `provider/id` and
    // turned into request fields by `thinking_body`.
    let thinking = cfg
        .thinking
        .iter()
        .find(|(spec, _)| *spec == format!("{}/{}", providers[provider].name, model))
        .map(|(_, level)| level.clone());
    validate_level(&providers[provider].cfg, thinking.as_deref());
    // An interactive run carries the selection in the live region's last row
    // and opens with a splash; a piped run owns no row and no screen, so the
    // banner is the only place the selection can appear.
    if ui::is_interactive() {
        banner(notice.as_deref());
    } else {
        // One leading blank: the same margin `ui::Screen` keeps on both edges.
        println!(" {}", selection(&providers, provider, &model, &thinking));
        if let Some(notice) = &notice {
            println!(" {notice}");
        }
    }

    // Kept apart from the generated prompt: `/explore` swaps the tools, so the
    // prompt is regenerated and these blocks are appended again unchanged.
    let mut instructions = String::new();
    if let Some(text) = &global_agents {
        instructions.push_str("\n\n# Instructions (global)\n\n");
        instructions.push_str(text);
    }
    if let Some(text) = &project_agents {
        instructions.push_str("\n\n# Instructions (project)\n\n");
        instructions.push_str(text);
    }
    if let Some(extra) = &cfg.system {
        instructions.push_str("\n\n");
        instructions.push_str(extra);
    }
    let mut system = tools::system_prompt(false);
    system.push_str(&instructions);
    let messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];

    let session = Session {
        cfg,
        providers,
        provider,
        model,
        thinking,
        thoughts: false,
        read_only: false,
        instructions,
        messages,
    };

    if ui::is_interactive() {
        session.run_interactive()
    } else {
        session.run_piped()
    }
}

/// Everything the REPL mutates between turns: the model selection and history.
struct Session {
    cfg: config::Config,
    providers: Vec<Provider>,
    provider: usize,
    model: String,
    thinking: Option<String>,
    /// Show reasoning in the transcript (`/thoughts`). Off by default: the
    /// status row's "thinking…" is enough unless the user asks for more.
    thoughts: bool,
    /// Explore mode: `read` and a read-only `bash` only (`/explore`).
    read_only: bool,
    /// The config and project instructions appended to the generated prompt,
    /// kept so a mode switch can rebuild the first message.
    instructions: String,
    messages: Vec<Value>,
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

/// What the prompt yielded: a line, or a mode toggle that is not one.
enum Input {
    /// The committed line.
    Line(String),
    /// Shift+Tab: explore mode, handled without touching the history.
    Explore,
}

/// Slash commands, for Tab completion in the prompt. Only the name is
/// completed; `/model` and `/thinking` take their argument as typed.
const COMMANDS: &[&str] = &[
    "/exit",
    "/explore",
    "/help",
    "/model",
    "/models",
    "/quit",
    "/thinking",
    "/thoughts",
];

impl Session {
    /// One line at a time from a terminal we own. Keys arrive on the channel, so
    /// a turn in flight never blocks the prompt.
    fn run_interactive(mut self) -> Result<(), String> {
        let mut screen =
            ui::Screen::new("❯ ").map_err(|e| format!("cannot take the terminal: {e}"))?;
        screen.set_commands(COMMANDS);
        screen.set_info(&self.info());
        let (tx, rx) = mpsc::channel();
        spawn_input(tx.clone());

        // A line submitted mid-turn waits here until the turn boundary, where it
        // is dispatched exactly like a line typed while idle. That is why slash
        // commands are handled in this loop and not only where they are parsed.
        let mut pending: Option<String> = None;
        loop {
            let line = match pending.take() {
                Some(line) => line,
                None => match readline(&mut screen, &rx) {
                    Some(Input::Line(line)) => line,
                    Some(Input::Explore) => {
                        self.toggle_explore(&mut screen);
                        continue;
                    }
                    None => break,
                },
            };
            match self.dispatch(&line, &mut screen, &rx) {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    pending = self.messages_turn(&input, &tx, &rx, &mut screen)?;
                }
                Cmd::Shell(command) => self.shell(&command, &mut screen),
            }
        }
        screen.close();
        Ok(())
    }

    /// No tty: the kernel edits the line and the turn prints as it streams.
    fn run_piped(mut self) -> Result<(), String> {
        loop {
            let Some(line) = ui::cooked_line("❯ ") else {
                break;
            };
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
                    self.messages.push(json!({ "role": "user", "content": input }));
                    if let Err(e) = self.turn(&mut sink, &cancel) {
                        eprintln!("nano: {e}");
                    }
                }
                Cmd::Shell(command) => {
                    println!("{}", plain_text(&shell_line(&command, false)));
                    let result = self.run_shell(&command);
                    print!("{result}");
                }
            }
        }
        Ok(())
    }

    /// The live region's last row: what this session is pointed at.
    fn info(&self) -> String {
        let base = selection(&self.providers, self.provider, &self.model, &self.thinking);
        if self.read_only {
            format!("{base}  ·  read-only")
        } else {
            base
        }
    }

    /// Enter or leave explore mode. The tool set changes, and the generated
    /// prompt changes with it: the first message is rebuilt in place, so the
    /// history after it is untouched.
    fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
        let mut system = tools::system_prompt(read_only);
        system.push_str(&self.instructions);
        self.messages[0] = json!({ "role": "system", "content": system });
    }

    /// `/explore` and Shift+Tab share one path, so the prompt color, the
    /// selection row and the transcript never disagree about the mode.
    fn toggle_explore(&mut self, screen: &mut ui::Screen) {
        self.set_read_only(!self.read_only);
        screen.set_explore(self.read_only);
        screen.set_info(&self.info());
        screen.output(&format!(
            "explore: {}\n",
            if self.read_only { "on" } else { "off" }
        ));
    }

    /// `!cmd`: run it here, show it, and let the output join the history. No
    /// turn follows — the model sees it when the user next asks something.
    fn shell(&mut self, command: &str, screen: &mut ui::Screen) {
        screen.insert(vec![line_of(&shell_line(command, false))], ui::Block::Tool);
        let _ = screen.draw();
        let result = self.run_shell(command);
        screen.output(&result);
        let _ = screen.draw();
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
        self.messages.push(json!({
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
        self.messages.push(json!({ "role": "user", "content": input }));

        let cancel = http::Cancel::new();
        let mut queued: Option<String> = None;
        // Streamed text up to the last newline is permanent; the rest is the
        // line still being written and lives in the status row.
        let mut partial = String::new();
        // Reasoning before the answer. Its last line is unfinished until the
        // answer starts or the round ends, so it is held until then.
        let mut thought = String::new();

        screen.set_status("thinking…");
        let _ = screen.draw();

        let messages = &mut self.messages;
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        let thoughts = self.thoughts;
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
                    thinking.as_deref(),
                    read_only,
                    &mut sink,
                    &turn_cancel,
                );
                let _ = turn_tx.send(Event::Done(result));
            });

            loop {
                // Block for the first event, then take everything already
                // queued before drawing once: a repaint per token is a waste.
                let first = match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(ev) => Some(ev),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if handle.is_finished() {
                            break;
                        }
                        None
                    }
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
                                    queued = Some(screen.commit());
                                    screen.set_queued(queued.as_deref());
                                    dirty = true;
                                }
                            }
                            // The turn already captured its tool set; a mode
                            // switch takes effect on the next one.
                            ui::Action::Explore => {}
                            ui::Action::None => dirty = true,
                        },
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
                            screen.set_status(&stripped(&partial));
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
                        Event::Done(Err(e)) => {
                            screen.error(&format!("nano: {e}\n"));
                            done = true;
                        }
                        Event::Done(Ok(())) => {
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
            thinking.as_deref(),
            self.read_only,
            sink,
            cancel,
        )
    }

    /// Slash commands and selection changes. `screen` supplies a line from the
    /// terminal for the model picker.
    fn dispatch(
        &mut self,
        line: &str,
        screen: &mut ui::Screen,
        rx: &Receiver<Event>,
    ) -> Cmd {
        let input = line.trim();
        if input.is_empty() {
            return Cmd::Continue;
        }
        let providers = &self.providers;
        let (provider, model) = (self.provider, &self.model);

        match input {
            "/exit" | "/quit" => return Cmd::Exit,
            "/help" => {
                screen.output(
                    "/model [id]    pick or switch model\n\
                     /models        list models\n\
                     /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                     /thoughts      show or hide the model's reasoning\n\
                     /explore       toggle read-only mode (read and read-only shell)\n\
                     !cmd           run a shell command; its output joins the context\n\
                     /exit          quit\n",
                );
                return Cmd::Continue;
            }
            "/models" => {
                let text = models(&providers);
                screen.output(&text);
                return Cmd::Continue;
            }
            "/model" => {
                if let Some((pi, id)) = pick(providers, (provider, model), screen, rx) {
                    self.provider = pi;
                    self.model = id;
                    let text = self.info();
                    screen.set_info(&text);
                    screen.output(&format!("{text}\n"));
                    save_selection(&providers[self.provider].name, &self.model);
                }
                return Cmd::Continue;
            }
            _ => {}
        }

        if input == "/thinking" || input.starts_with("/thinking ") {
            let rest = input.strip_prefix("/thinking").unwrap_or("").trim();
            if rest.is_empty() {
                screen.output(&format!(
                    "thinking: {}\n\n",
                    thinking_label(&providers[self.provider].cfg, &self.thinking)
                ));
                return Cmd::Continue;
            }
            if let Some(level) = set_thinking(&providers[self.provider].cfg, rest) {
                self.thinking = Some(level.clone());
                screen.set_info(&self.info());
                let spec = format!("{}/{}", providers[self.provider].name, self.model);
                config::save_thinking(&spec, level.as_str());
                screen.output(&format!(
                    "thinking: {}\n\n",
                    thinking_label(&providers[self.provider].cfg, &self.thinking)
                ));
            }
            return Cmd::Continue;
        }

        if let Some(rest) = input.strip_prefix("/model ") {
            let spec = rest.trim();
            match find(providers, spec) {
                Some((pi, id)) => {
                    self.provider = pi;
                    self.model = id;
                    self.thinking = self
                        .cfg
                        .thinking
                        .iter()
                        .find(|(spec, _)| {
                            *spec == format!("{}/{}", providers[self.provider].name, self.model)
                        })
                        .map(|(_, level)| level.clone());
                    let text = self.info();
                    screen.set_info(&text);
                    screen.output(&format!("{text}\n"));
                    save_selection(&providers[self.provider].name, &self.model);
                }
                None => screen.error(&format!("unknown model: {spec}\n")),
            }
            return Cmd::Continue;
        }

        if input == "/explore" {
            self.toggle_explore(screen);
            return Cmd::Continue;
        }

        if input == "/thoughts" {
            self.thoughts = !self.thoughts;
            screen.output(&format!(
                "thoughts: {}\n\n",
                if self.thoughts { "on" } else { "off" }
            ));
            return Cmd::Continue;
        }

        if input.starts_with('/') {
            screen.error(&format!("unknown command: {input}\n"));
            return Cmd::Continue;
        }

        if let Some(command) = shell_command(input) {
            return command;
        }

        Cmd::Turn(input.to_string())
    }
}

/// The same dispatch for a terminal that is not ours: commands only, warnings
/// on stderr, no picker.
fn dispatch_piped(session: &mut Session, line: &str) -> Cmd {
    let input = line.trim();
    let providers = &session.providers;
    match input {
        "/exit" | "/quit" => return Cmd::Exit,
        "/help" => {
            println!(
                "/model [id]    pick or switch model\n\
                 /models        list models\n\
                 /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                 /thoughts      show or hide the model's reasoning\n\
                 /explore       toggle read-only mode (read and read-only shell)\n\
                 !cmd           run a shell command; its output joins the context\n\
                 /exit          quit"
            );
            return Cmd::Continue;
        }
        "/models" => {
            println!("{}", models(providers).trim_end());
            return Cmd::Continue;
        }
        "/model" => {
            eprintln!("nano: /model needs a terminal");
            return Cmd::Continue;
        }
        // Reasoning goes to stdout in a piped run; only the live region can
        // hide it, so the toggle means nothing here.
        "/thoughts" => {
            eprintln!("nano: /thoughts needs a terminal");
            return Cmd::Continue;
        }
        "/explore" => {
            session.set_read_only(!session.read_only);
            println!("explore: {}", if session.read_only { "on" } else { "off" });
            return Cmd::Continue;
        }
        _ => {}
    }
    if let Some(rest) = input.strip_prefix("/model ") {
        match find(providers, rest.trim()) {
            Some((pi, id)) => {
                session.provider = pi;
                session.model = id;
                session.thinking = session
                    .cfg
                    .thinking
                    .iter()
                    .find(|(spec, _)| {
                        *spec == format!("{}/{}", providers[session.provider].name, session.model)
                    })
                    .map(|(_, level)| level.clone());
                println!(
                    "{}",
                    selection(
                        providers,
                        session.provider,
                        &session.model,
                        &session.thinking
                    )
                );
                save_selection(&providers[session.provider].name, &session.model);
            }
            None => eprintln!("nano: unknown model: {}", rest.trim()),
        }
        return Cmd::Continue;
    }
    if input == "/thinking" || input.starts_with("/thinking ") {
        let rest = input.strip_prefix("/thinking").unwrap_or("").trim();
        if rest.is_empty() {
            println!(
                "thinking: {}",
                thinking_label(&providers[session.provider].cfg, &session.thinking)
            );
            return Cmd::Continue;
        }
        if let Some(level) = set_thinking(&providers[session.provider].cfg, rest) {
            session.thinking = Some(level.clone());
            let spec = format!("{}/{}", providers[session.provider].name, session.model);
            config::save_thinking(&spec, level.as_str());
        }
        return Cmd::Continue;
    }
    if input.starts_with('/') {
        eprintln!("nano: unknown command: {input}");
        return Cmd::Continue;
    }
    if let Some(command) = shell_command(input) {
        return command;
    }
    Cmd::Turn(input.to_string())
}

/// `!cmd`: a command for this machine, not a message for the model. A bare
/// `!` is nothing to run.
fn shell_command(input: &str) -> Option<Cmd> {
    let command = input.strip_prefix('!')?.trim();
    if command.is_empty() {
        return Some(Cmd::Continue);
    }
    Some(Cmd::Shell(command.to_string()))
}

/// Events from the two producers that never draw: keys, and the turn.
enum Event {
    Key(ratatui::crossterm::event::KeyEvent),
    /// Transcript text: finished model lines, already spaced.
    Printed(String),
    /// One tool call and its outcome, as styled spans, and the failure's own
    /// words when it failed.
    Echo(Vec<(Paint, String)>, Option<String>),
    /// A transient label for the status row.
    Status(String),
    /// Streamed reasoning, before any answer text.
    Thought(String),
    Done(Result<(), String>),
    Eof,
}

/// Blocking `read` for the terminal's keys. Nothing else may read stdin.
fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        use ratatui::crossterm::event::{read, Event as TermEvent};
        loop {
            match read() {
                // Key releases (kitty protocol) are not new input.
                Ok(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    if tx.send(Event::Key(key)).is_err() {
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
                // Enter on an empty line commits nothing: a bare prompt in the
                // transcript is noise, and an empty line is not input.
                ui::Action::Submit if screen.is_empty() => {}
                ui::Action::Submit => return Some(Input::Line(screen.commit())),
                ui::Action::Explore => return Some(Input::Explore),
                ui::Action::Eof => return None,
                ui::Action::None => {
                    let _ = screen.draw();
                }
            },
            Ok(Event::Eof) | Err(_) => return None,
            // A turn's events cannot arrive while idle; drop them if they do.
            Ok(_) => {}
        }
    }
}

/// Print text that arrives outside the drawing loop (a slash command's output).
/// It is permanent, exactly like streamed text.
/// A drawn row, so it must not carry the newlines that end transcript lines.
fn stripped(text: &str) -> String {
    text.trim_start_matches(['\n', '\r']).replace(['\n', '\r'], " ")
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
    /// A transient label for the live region: "thinking…", a running command.
    fn status(&mut self, text: &str);
    /// One tool call, finished: the call and its outcome on one line, and the
    /// failure's own words beneath it.
    fn echo(
        &mut self,
        call: &[(Paint, String)],
        mark: (Paint, &'static str),
        error: Option<&str>,
    );
    /// A round's text is over; close its last line if it wrote one.
    fn end_round(&mut self);
}

#[derive(Default)]
struct PlainSink {
    out: TextSink,
    md: md::Stream,
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
            buf.push_str(&md::ansi(line));
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

    fn echo(
        &mut self,
        call: &[(Paint, String)],
        mark: (Paint, &'static str),
        error: Option<&str>,
    ) {
        self.flush();
        let mut line = String::new();
        for (paint, text) in call {
            line.push_str(&paint.ansi(text));
        }
        line.push(' ');
        line.push_str(&mark.0.ansi(mark.1));
        print!("{line}\n");
        if let Some(error) = error {
            print!("    {}\n", Paint::Red.ansi(error));
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

    fn echo(
        &mut self,
        call: &[(Paint, String)],
        mark: (Paint, &'static str),
        error: Option<&str>,
    ) {
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
            self.send(Event::Status("thinking…".to_string()));
        }
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
    /// plain.
    fn ansi(self, text: &str) -> String {
        let code = match self {
            Paint::Plain => return text.to_string(),
            Paint::Dim => "2",
            Paint::Cyan => "36",
            Paint::Yellow => "33",
            Paint::Green => "32",
            Paint::Red => "31",
        };
        if io::stdout().is_terminal() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
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
fn set_thinking(provider: &config::ProviderConfig, input: &str) -> Option<String> {
    let names = level_names(provider);
    let level = names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(input))
        .cloned();
    if level.is_none() {
        eprintln!(
            "nano: unknown thinking level: {input} (known: {})",
            names.join(", ")
        );
    }
    level
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
    for line in LOGO {
        println!(" {}", Paint::Cyan.ansi(line));
    }
    if let Some(note) = note {
        println!();
        println!(" {}", Paint::Dim.ansi(note));
    }
    println!();
}

/// Persist the current selection so the next run starts on the same model.
fn save_selection(provider: &str, model: &str) {
    config::save_model(&format!("{provider}/{model}"));
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

fn run_turn(
    provider: &Provider,
    model: &str,
    messages: &mut Vec<Value>,
    thinking: Option<&str>,
    read_only: bool,
    sink: &mut dyn Sink,
    cancel: &http::Cancel,
) -> Result<(), String> {
    let tools = tools::definitions(read_only);

    loop {
        // An interrupt ends the turn here too: no further round is started.
        // The user message stays without a reply, which is what happened.
        if cancel.cancelled() {
            return Ok(());
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
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        sink.end_round();
        // A shut-down read can also end the stream without an error. A cut
        // reply is not an assistant message, and its calls never run.
        if cancel.cancelled() {
            return Ok(());
        }

        if reply.calls.is_empty() {
            messages.push(assistant_message(&reply));
            return Ok(());
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
        messages.push(message);

        for call in &reply.calls {
            if cancel.cancelled() {
                return Ok(());
            }
            let echo = call_line(call, read_only);
            // What is running, while it runs: the finished line appears below
            // once there is a result to put on it.
            sink.status(&plain_text(&echo));
            let result = tools::execute(&call.name, &call.arguments, cancel, read_only);
            let error = failed(&result).then(|| detail(&result));
            sink.echo(&echo, mark(&result), error.as_deref());
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result,
            }));
            if cancel.cancelled() {
                return Ok(());
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
    let paint = if read_only { Paint::Cyan } else { Paint::Yellow };
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
        "write" => ("✚", Paint::Yellow),
        "edit" => ("✎", Paint::Yellow),
        _ => ("✚", Paint::Yellow),
    };

    if call.name == "bash" {
        let command = v["command"].as_str().unwrap_or("bash");
        return shell_line(command, read_only);
    }

    let mut parts = Vec::new();
    for key in ["path", "file", "pattern"] {
        if let Some(s) = v[key].as_str() {
            parts.push(summarize(s, 120));
        }
    }
    // A partial read is only reproducible with its window, so echo it.
    let num = |key: &str| v[key].as_u64().map(|n| n.to_string());
    let (offset, limit) = (num("offset"), num("limit"));
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
