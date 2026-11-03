mod api;
mod config;
mod http;
mod term;
mod tools;

use std::cell::RefCell;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use termion::event::Key;

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
        warn(&format!("nano: {e}\n"));
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
    let project_agents = project_file.as_deref().and_then(|f| {
        let text = config::read_agents(f)?;
        eprintln!("nano: using project instructions from {}", f.display());
        Some(text)
    });

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
    print_selection(&providers, provider, &model, &thinking);

    let mut system = tools::system_prompt();
    if let Some(text) = &global_agents {
        system.push_str("\n\n# Instructions (global)\n\n");
        system.push_str(text);
    }
    if let Some(text) = &project_agents {
        system.push_str("\n\n# Instructions (project)\n\n");
        system.push_str(text);
    }
    if let Some(extra) = &cfg.system {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    let messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];

    let session = Session {
        cfg,
        providers,
        provider,
        model,
        thinking,
        messages,
    };

    if term::is_interactive() {
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
    messages: Vec<Value>,
}

/// What a submitted line asks for.
enum Cmd {
    Exit,
    /// Handled already, or nothing to do.
    Continue,
    /// Send this to the model.
    Turn(String),
}

impl Session {
    /// One line at a time from a terminal we own. Keys arrive on the channel, so
    /// a turn in flight never blocks the prompt.
    fn run_interactive(mut self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        spawn_input(tx.clone());

        let mut editor = term::Editor::new().open();
        // A line submitted mid-turn waits here until the turn boundary, where it
        // is dispatched exactly like a line typed while idle. That is why slash
        // commands are handled in this loop and not only where they are parsed.
        let mut pending: Option<String> = None;
        loop {
            let line = match pending.take() {
                Some(line) => line,
                None => {
                    // Erase first: the previous drawing still sits on screen,
                    // and a newline under it would leave a phantom prompt.
                    editor.erase();
                    term::out("\n");
                    let Some(line) = read_line(&mut editor, &rx) else {
                        break;
                    };
                    line
                }
            };
            match self.dispatch(&line, &mut |prompt| {
                // A command's own prompt is temporary: restore the REPL's once
                // the line is in.
                let saved = editor.prompt().to_string();
                editor.set_prompt(prompt);
                let line = read_line(&mut editor, &rx);
                editor.set_prompt(&saved);
                line
            }) {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    pending = self.messages_turn(&input, &tx, &rx, &mut editor)?;
                }
            }
        }
        Ok(())
    }

    /// No tty: the kernel edits the line and the turn prints as it streams.
    fn run_piped(mut self) -> Result<(), String> {
        loop {
            let Some(line) = term::cooked_line("> ") else {
                break;
            };
            match self.dispatch(&line, &mut |prompt| term::cooked_line(prompt)) {
                Cmd::Exit => break,
                Cmd::Continue => {}
                Cmd::Turn(input) => {
                    let mut sink = PlainSink::default();
                    let cancel = AtomicBool::new(false);
                    self.messages.push(json!({ "role": "user", "content": input }));
                    if let Err(e) = self.turn(&mut sink, &cancel) {
                        warn(&format!("nano: {e}\n"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Run one turn on its own thread while this one keeps drawing.
    fn messages_turn(
        &mut self,
        input: &str,
        tx: &Sender<Event>,
        rx: &Receiver<Event>,
        editor: &mut term::Editor,
    ) -> Result<Option<String>, String> {
        self.messages.push(json!({ "role": "user", "content": input }));

        let cancel = Arc::new(AtomicBool::new(false));
        let mut queued: Option<String> = None;
        // Streamed text up to the last newline is permanent; the rest is the
        // line still being written and is drawn, not printed.
        let mut partial = String::new();

        editor.set_status("thinking…");
        editor.draw();

        let messages = &mut self.messages;
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        let turn_tx = tx.clone();
        let turn_cancel = cancel.clone();

        let result = thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let mut sink = ChannelSink {
                    tx: turn_tx.clone(),
                    out: TextSink::default(),
                };
                let result = run_turn(
                    provider,
                    &model,
                    messages,
                    thinking.as_deref(),
                    &mut sink,
                    &turn_cancel,
                );
                let _ = turn_tx.send(Event::Done(result));
            });

            loop {
                // Block for the first event, then take everything already
                // queued before drawing once: a repaint per token is unusable,
                // and the output is the same either way.
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
                        // the command that is running. The editor reports the
                        // key, the tool layer watches the flag.
                        Event::Key(Key::Esc) => tools::interrupt(),
                        Event::Key(key) => match editor.apply_key(key) {
                            term::Action::Eof => {
                                cancel.store(true, Ordering::SeqCst);
                                done = true;
                            }
                            term::Action::Submit => {
                                if !editor.is_empty() && queued.is_none() {
                                    queued = Some(editor.commit());
                                    editor.set_queued(queued.as_deref());
                                    dirty = true;
                                }
                            }
                            term::Action::None => dirty = true,
                        },
                        Event::Raw(text) => {
                            // Tool-call echoes are not model text: they must not
                            // disturb the unfinished line, only the drawing.
                            dirty = true;
                            editor.erase();
                            term::out(&format!("{text}\n"));
                        }
                        Event::Printed(text) => {
                            dirty = true;
                            partial.push_str(&text);
                            // Everything up to the last newline is permanent;
                            // what follows it is the line still being written.
                            let done = match partial.rfind('\n') {
                                Some(at) => partial.drain(..=at).collect::<String>(),
                                None => String::new(),
                            };
                            // Status first: `say` redraws, and it must redraw the
                            // new state, not the line it is about to print.
                            editor.set_status(&stripped(&partial));
                            if !done.is_empty() {
                                say(editor, &done);
                            }
                        }
                        Event::Reasoning => {
                            let label = stripped(&partial);
                            let label = if label.is_empty() {
                                "thinking…".to_string()
                            } else {
                                label
                            };
                            if editor.status() != label {
                                dirty = true;
                                editor.set_status(&label);
                            }
                        }
                        Event::Done(Err(e)) => {
                            editor.erase();
                            warn(&format!("nano: {e}\n"));
                            done = true;
                        }
                        Event::Done(Ok(())) => {
                            // `end_round` closed the last line, so nothing is
                            // left pending.
                            partial.clear();
                            done = true;
                        }
                        Event::Eof => {
                            cancel.store(true, Ordering::SeqCst);
                            done = true;
                        }
                    }
                    if done || cancel.load(Ordering::SeqCst) {
                        break;
                    }
                }
                if dirty {
                    editor.draw();
                }
                if done || cancel.load(Ordering::SeqCst) {
                    break;
                }
            }
            handle.join().is_ok()
        });
        editor.set_status("");
        editor.set_queued(None);

        if !result {
            warn("nano: turn failed\n");
        }
        Ok(queued)
    }

    fn turn(&mut self, sink: &mut dyn Sink, cancel: &AtomicBool) -> Result<(), String> {
        let provider = &self.providers[self.provider];
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        run_turn(
            provider,
            &model,
            &mut self.messages,
            thinking.as_deref(),
            sink,
            cancel,
        )
    }

    /// Slash commands and selection changes. `read` supplies a line from the
    /// terminal for the model picker; piped mode passes a no-op.
    fn dispatch(
        &mut self,
        line: &str,
        read: &mut dyn FnMut(&str) -> Option<String>,
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
                say_all(
                    "/model [id]    pick or switch model\n\
                     /models        list models\n\
                     /thinking [l]  show or set thinking level (off, low, medium, high)\n\
                     /exit          quit\n",
                );
                return Cmd::Continue;
            }
            "/models" => {
                print_models(providers);
                return Cmd::Continue;
            }
            "/model" => {
                if let Some((pi, id)) = pick(providers, (provider, model), read) {
                    self.provider = pi;
                    self.model = id;
                    print_selection(providers, self.provider, &self.model, &self.thinking);
                    save_selection(&providers[self.provider].name, &self.model);
                }
                return Cmd::Continue;
            }
            _ => {}
        }

        if input == "/thinking" || input.starts_with("/thinking ") {
            let rest = input.strip_prefix("/thinking").unwrap_or("").trim();
            if rest.is_empty() {
                say_all(&format!(
                    "thinking: {}\n\n",
                    thinking_label(&providers[self.provider].cfg, &self.thinking)
                ));
                return Cmd::Continue;
            }
            if let Some(level) = set_thinking(&providers[self.provider].cfg, rest) {
                self.thinking = Some(level.clone());
                let spec = format!("{}/{}", providers[self.provider].name, self.model);
                config::save_thinking(&spec, level.as_str());
                say_all(&format!(
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
                    print_selection(
                        providers,
                        self.provider,
                        &self.model,
                        &self.thinking,
                    );
                    save_selection(&providers[self.provider].name, &self.model);
                }
                None => eprintln!("unknown model: {spec}"),
            }
            return Cmd::Continue;
        }

        if input.starts_with('/') {
            warn(&format!("unknown command: {input}\n"));
            return Cmd::Continue;
        }

        Cmd::Turn(input.to_string())
    }
}

/// Events from the two producers that never draw: keys, and the turn.
enum Event {
    Key(Key),
    /// Finished model lines, printed permanently above the prompt.
    Printed(String),
    /// Raw output (a tool-call echo): permanent, but not model text.
    Raw(String),
    /// The model is reasoning; no answer text yet.
    Reasoning,
    Done(Result<(), String>),
    Eof,
}

/// Blocking `read` for the terminal's keys. Nothing else may read stdin.
fn spawn_input(tx: Sender<Event>) {
    thread::spawn(move || {
        use termion::input::TermRead;
        for event in io::stdin().events() {
            let Ok(termion::event::Event::Key(key)) = event else {
                continue;
            };
            if tx.send(Event::Key(key)).is_err() {
                return;
            }
        }
        let _ = tx.send(Event::Eof);
    });
}

/// Collect one line from the editor. Keys are the only input while idle.
fn read_line(editor: &mut term::Editor, rx: &Receiver<Event>) -> Option<String> {
    editor.draw();
    loop {
        match rx.recv() {
            Ok(Event::Key(key)) => match editor.apply_key(key) {
                term::Action::Submit => return Some(editor.commit()),
                term::Action::Eof => {
                    editor.erase();
                    return None;
                }
                term::Action::None => editor.draw(),
            },
            Ok(Event::Eof) | Err(_) => {
                editor.erase();
                return None;
            }
            // A turn's events cannot arrive while idle; drop them if they do.
            Ok(_) => {}
        }
    }
}

/// A drawn row, so it must not carry the newlines that end transcript lines.
fn stripped(text: &str) -> String {
    text.trim_start_matches(['\n', '\r']).replace(['\n', '\r'], " ")
}

/// A command's output. No drawing is on screen when one runs: the line was
/// committed before `dispatch` saw it.
fn say_all(text: &str) {
    term::out(text);
}

/// The same for warnings, which go to stderr but share the terminal.
fn warn(text: &str) {
    term::err(text);
}

fn say(editor: &mut term::Editor, text: &str) {
    if text.is_empty() {
        return;
    }
    // Permanent text: erase the drawing, write, redraw underneath.
    editor.erase();
    term::out(text);
    editor.draw();
}

/// Where a turn's output goes: straight to stdout when piped, onto the channel
/// when a prompt is on screen.
trait Sink {
    /// Streamed model text. `TextSink` owns the spacing: finished lines go out
    /// permanently, the unfinished one is exposed as a tail.
    fn text(&mut self, text: &str);
    /// Raw output: no spacing decisions, no trailing newline. Goes out
    /// permanently, so a tool-call line is never lost to a redrawn frame.
    fn append(&mut self, text: &str);
    /// A round's text is over; close its last line if it wrote one.
    fn end_round(&mut self);
    fn reasoning(&mut self);
}

#[derive(Default)]
struct PlainSink {
    out: TextSink,
}

impl Sink for PlainSink {
    fn text(&mut self, text: &str) {
        let mut done = String::new();
        self.out.push(text, &mut done);
        print!("{done}");
        io::stdout().flush().ok();
    }

    fn append(&mut self, text: &str) {
        print!("{text}");
        io::stdout().flush().ok();
    }

    fn end_round(&mut self) {
        self.out.commit();
        if std::mem::take(&mut self.out.wrote) {
            println!();
        }
    }

    fn reasoning(&mut self) {}
}

struct ChannelSink {
    tx: Sender<Event>,
    out: TextSink,
}

impl ChannelSink {
    /// Permanent text first, then the unfinished line: one order, always.
    fn publish(&self, done: String) {
        if !done.is_empty() {
            let _ = self.tx.send(Event::Printed(done));
        }
    }
}

impl Sink for ChannelSink {
    fn text(&mut self, text: &str) {
        let mut done = String::new();
        self.out.push(text, &mut done);
        self.publish(done);
    }

    fn append(&mut self, text: &str) {
        if !text.is_empty() {
            let _ = self.tx.send(Event::Raw(text.to_string()));
        }
    }

    fn end_round(&mut self) {
        self.out.commit();
        if std::mem::take(&mut self.out.wrote) {
            // The reply ended without a final newline, so the line it left
            // unfinished becomes permanent here.
            self.publish("\n".to_string());
        }
    }

    fn reasoning(&mut self) {
        let _ = self.tx.send(Event::Reasoning);
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
fn set_thinking(provider: &config::ProviderConfig, input: &str) -> Option<String> {
    let names = level_names(provider);
    let level = names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(input))
        .cloned();
    if level.is_none() {
        warn(&format!("unknown thinking level: {input} (known: {})\n", names.join(", ")));
    }
    level
}

/// Warn once at startup if the config remembers a level the provider dropped.
fn validate_level(provider: &config::ProviderConfig, level: Option<&str>) {
    let Some(level) = level else { return };
    if !level_names(provider).iter().any(|name| name == level) {
        warn(&format!(
            "{}: thinking level \"{level}\" is not configured, using default\n",
            provider.name
        ));
    }
}

fn print_selection(
    providers: &[Provider],
    provider: usize,
    model: &str,
    thinking: &Option<String>,
) {
    let level = match thinking {
        Some(level) => level.clone(),
        None => "default".to_string(),
    };
    say_all(&format!(
        "model: {}/{}\nthinking: {}\n",
        providers[provider].name, model, level
    ));
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

fn print_models(providers: &[Provider]) {
    let mut text = String::new();
    for p in providers {
        text.push_str(&format!("{}:\n", p.name));
        for m in &p.models {
            match m.context {
                Some(ctx) => text.push_str(&format!("  {}/{}  ({} ctx)\n", p.name, m.id, ctx)),
                None => text.push_str(&format!("  {}/{}", p.name, m.id)),
            }
            if m.context.is_none() {
                text.push('\n');
            }
        }
    }
    say_all(&text);
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
        warn(&format!("model {spec:?} not available, using default\n"));
    }
    (0, providers[0].models[0].id.clone())
}

fn pick(
    providers: &[Provider],
    current: (usize, &str),
    read: &mut dyn FnMut(&str) -> Option<String>,
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

    say_all(&text);
    let line = read("model> ")?;
    let choice = line.trim();
    if choice.is_empty() {
        return None;
    }

    if let Ok(n) = choice.parse::<usize>() {
        return match items.get(n.wrapping_sub(1)) {
            Some(hit) if n >= 1 => Some(hit.clone()),
            _ => {
                warn(&format!("no model #{n}\n"));
                None
            }
        };
    }

    match find(providers, choice) {
        Some(hit) => Some(hit),
        None => {
            warn(&format!("unknown model: {choice}\n"));
            None
        }
    }
}

fn run_turn(
    provider: &Provider,
    model: &str,
    messages: &mut Vec<Value>,
    thinking: Option<&str>,
    sink: &mut dyn Sink,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let tools = tools::definitions();

    loop {
        // Quitting ends the turn here: no further round is started once the
        // terminal is gone. The user message stays; there is no reply to pair
        // it with, which is exactly what happened.
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        let extra = provider.cfg.thinking_body(thinking);
        // Both callbacks borrow the sink, so it is shared rather than captured
        // twice.
        let reply = {
            let sink = RefCell::new(&mut *sink);
            api::stream_chat(
                &provider.ep,
                provider.key.as_deref(),
                &provider.headers,
                model,
                messages,
                tools,
                extra.as_ref(),
                |text| sink.borrow_mut().text(text),
                || sink.borrow_mut().reasoning(),
            )?
        };
        sink.end_round();

        if reply.calls.is_empty() {
            messages.push(json!({ "role": "assistant", "content": reply.text }));
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
        messages.push(json!({
            "role": "assistant",
            "content": reply.text,
            "tool_calls": calls,
        }));

        for call in &reply.calls {
            if cancel.load(Ordering::SeqCst) {
                return Ok(());
            }
            sink.append(&call_line(call));
            let result = tools::execute(&call.name, &call.arguments);
            sink.append(&format!(" {}", mark(&result)));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result,
            }));
            if tools::take_interrupt() {
                return Ok(());
            }
        }
    }
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
    /// mean nothing appears until the line ends. `out` must be permanent, not
    /// part of a redrawn frame: frames that scroll off the top are gone.
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
                out.push_str("\n\n");
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

// Echo tool calls compactly: show the arguments that identify the target, never
// the raw JSON (heredoc scripts and long content are unreadable in a line).
// A glyph per tool says what it does at a glance: read-only is dim, anything
// that can change the tree is yellow. `bash` has no call syntax; its command
// stands alone.
fn call_line(call: &api::ToolCall) -> String {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let (glyph, code) = match call.name.as_str() {
        "read" => ("◇", "2"),
        "write" => ("✚", "33"),
        "edit" => ("✎", "33"),
        "bash" => ("❯", "33"),
        _ => ("◆", "33"),
    };
    if call.name == "bash" {
        let cmd = v["command"].as_str().map(|s| summarize(s, 120));
        return match cmd {
            Some(cmd) => format!("    {} {}", paint(code, glyph), paint(code, &cmd)),
            None => format!("    {} {}", paint(code, glyph), paint(code, "bash")),
        };
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
    // Read-only calls are dim; anything that can change the tree is yellow.
    let name = paint(code, &call.name);
    let glyph = paint(code, glyph);
    if parts.is_empty() {
        format!("    {glyph} {name}")
    } else {
        format!("    {glyph} {name}({})", parts.join(" "))
    }
}

/// One glyph for the outcome. A failed tool result is a result, not an error:
/// tools report failures as text, either `error: ...` or a non-zero `exit: N`.
fn mark(result: &str) -> String {
    let failed = if let Some(rest) = result.strip_prefix("exit: ") {
        let code = rest.lines().next().unwrap_or("").trim();
        code != "0"
    } else {
        result.starts_with("error:")
    };
    if failed {
        paint("31", "✗")
    } else {
        paint("32", "✓")
    }
}

/// ANSI colors only when stdout is a terminal, so redirected output stays plain.
fn paint(code: &str, text: &str) -> String {
    if io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn summarize(text: &str, max: usize) -> String {    let line = text.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.lines().count() > 1 {
        out.push('…');
    }
    out
}
