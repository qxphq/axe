mod api;
mod config;
mod http;
mod tools;

use std::io::{self, BufRead, IsTerminal, Read, Write};

use serde_json::{json, Value};

struct Provider {
    name: String,
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

    print_models(&providers);

    let (mut provider, mut model) = resolve(&providers, cfg.model.as_deref());
    println!("model: {}/{}\n", providers[provider].name, model);

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
    let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system })];

    let stdin = io::stdin();
    loop {
        println!();
        print!("{}", paint("36", "> "));
        io::stdout().flush().ok();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        redraw_input("> ", input);

        match input {
            "/exit" | "/quit" => break,
            "/help" => {
                println!("/model [id]  pick or switch model");
                println!("/models      list models");
                println!("/exit        quit");
                continue;
            }
            "/models" => {
                print_models(&providers);
                continue;
            }
            "/model" => {
                if let Some((pi, id)) = pick(&providers, (provider, &model)) {
                    provider = pi;
                    model = id;
                    println!("model: {}/{}\n", providers[provider].name, model);
                    save_selection(&providers[provider].name, &model);
                }
                continue;
            }
            _ => {}
        }

        if let Some(rest) = input.strip_prefix("/model ") {
            let spec = rest.trim();
            match find(&providers, spec) {
                Some((pi, id)) => {
                    provider = pi;
                    model = id;
                    println!("model: {}/{}\n", providers[provider].name, model);
                    save_selection(&providers[provider].name, &model);
                }
                None => eprintln!("unknown model: {spec}"),
            }
            continue;
        }

        if input.starts_with('/') {
            eprintln!("unknown command: {input}");
            continue;
        }

        messages.push(json!({ "role": "user", "content": input }));
        if let Err(e) = run_turn(&providers[provider], &model, &mut messages) {
            eprintln!("nano: {e}");
        }
    }

    Ok(())
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
    for p in providers {
        println!("{}:", p.name);
        for m in &p.models {
            match m.context {
                Some(ctx) => println!("  {}/{}  ({} ctx)", p.name, m.id, ctx),
                None => println!("  {}/{}", p.name, m.id),
            }
        }
    }
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
        eprintln!("model {spec:?} not available, using default");
    }
    (0, providers[0].models[0].id.clone())
}

fn pick(providers: &[Provider], current: (usize, &str)) -> Option<(usize, String)> {
    let mut items: Vec<(usize, String)> = Vec::new();
    for (pi, p) in providers.iter().enumerate() {
        for m in &p.models {
            let mark = if pi == current.0 && m.id == current.1 {
                "*"
            } else {
                " "
            };
            println!("{mark}{:>2}) {}/{}", items.len() + 1, p.name, m.id);
            items.push((pi, m.id.clone()));
        }
    }

    print!("model> ");
    io::stdout().flush().ok();

    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).ok()? == 0 {
        return None;
    }
    let choice = line.trim();
    if choice.is_empty() {
        return None;
    }

    if let Ok(n) = choice.parse::<usize>() {
        return match items.get(n.wrapping_sub(1)) {
            Some(hit) if n >= 1 => Some(hit.clone()),
            _ => {
                eprintln!("no model #{n}");
                None
            }
        };
    }

    match find(providers, choice) {
        Some(hit) => Some(hit),
        None => {
            eprintln!("unknown model: {choice}");
            None
        }
    }
}

fn run_turn(
    provider: &Provider,
    model: &str,
    messages: &mut Vec<Value>,
) -> Result<(), String> {
    let tools = tools::definitions();

    loop {
        println!();
        let mut out = TextSink::default();
        let reply = api::stream_chat(
            &provider.ep,
            provider.key.as_deref(),
            &provider.headers,
            model,
            messages,
            tools,
            |text| out.push(text),
        )?;

        if out.wrote {
            println!();
        }

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
            print!("{}", call_line(call));
            io::stdout().flush().ok();
            let result = tools::execute(&call.name, &call.arguments);
            println!(" {}", mark(&result));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": result,
            }));
            if tools::take_interrupt() {
                eprintln!("nano: turn interrupted, you can type again");
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
    pending: usize,
    wrote: bool,
}

impl TextSink {
    fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                if self.started {
                    self.pending += 1;
                }
                continue;
            }
            if !self.started {
                self.started = true;
            }
            for _ in 0..self.pending {
                print!("\n");
            }
            self.pending = 0;
            self.wrote = true;
            print!("{ch}");
        }
        io::stdout().flush().ok();
    }
}

// Echo tool calls compactly: show the arguments that identify the target, never
// the raw JSON (heredoc scripts and long content are unreadable in a line).
fn call_line(call: &api::ToolCall) -> String {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    let mut parts = Vec::new();
    for key in ["path", "command", "file", "pattern"] {
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
    let (glyph, code) = if call.name == "read" {
        ("◇", "2")
    } else {
        ("◆", "33")
    };
    let name = paint(code, &call.name);
    if parts.is_empty() {
        format!("{glyph} {name}")
    } else {
        format!("{glyph} {name}({})", parts.join(" "))
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

/// The terminal already echoed our input in its default color. Rewrite those
/// lines in color so user text is distinguishable from model output. The
/// cursor sits alone on the last row; redraw the prompt and text there and
/// erase the rows the echo wrapped into, bottom-up so nothing scrolls.
fn redraw_input(prompt: &str, text: &str) {
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return;
    }
    let cols = tty_columns();
    // Columns occupied by the prompt plus the echoed text, wrapping as the
    // terminal did it.
    let width = prompt.chars().count() + text.chars().count();
    let rows = width.div_ceil(cols).max(1);
    print!("\r\x1b[{rows}A\x1b[0J");
    print!("{}{}\n", paint("36", prompt), paint("36", text));
    io::stdout().flush().ok();
}

/// Terminal width for the redraw. `COLUMNS` is not exported by the shell, so
/// ask the tty; 80 only when both fail.
fn tty_columns() -> usize {
    let out = std::process::Command::new("stty")
        .arg("-F")
        .arg("/dev/tty")
        .arg("size")
        .output();
    out.ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(1)
                .and_then(|n| n.parse::<usize>().ok())
        })
        .filter(|n| *n > 0)
        .unwrap_or(80)
}

fn summarize(text: &str, max: usize) -> String {    let line = text.lines().next().unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || text.lines().count() > 1 {
        out.push('…');
    }
    out
}
