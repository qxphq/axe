mod api;
mod config;
mod http;

use std::io::{self, BufRead, Read, Write};

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

    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &cfg.system {
        messages.push(json!({ "role": "system", "content": system }));
    }

    let stdin = io::stdin();
    loop {
        print!("> ");
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
        match stream_reply(&providers[provider], &model, &messages) {
            Ok(reply) => {
                println!();
                messages.push(json!({ "role": "assistant", "content": reply }));
            }
            Err(e) => {
                println!();
                eprintln!("nano: {e}");
            }
        }
    }

    Ok(())
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

fn stream_reply(provider: &Provider, model: &str, messages: &[Value]) -> Result<String, String> {
    let mut res = api::chat(
        &provider.ep,
        provider.key.as_deref(),
        &provider.headers,
        model,
        messages,
    )?;
    if res.status != 200 {
        let body = res.read_all().unwrap_or_default();
        return Err(format!("HTTP {}: {}", res.status, body.trim()));
    }

    let stdout = io::stdout();
    let mut reply = String::new();

    loop {
        let line = match res.read_line() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => return Err(format!("read stream: {e}")),
        };
        let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if line.is_empty() {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
            print!("{text}");
            stdout.lock().flush().ok();
            reply.push_str(text);
        }
    }

    Ok(reply)
}
