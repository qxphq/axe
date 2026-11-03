use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Spec {
    name: &'static str,
    signature: &'static str,
    description: &'static str,
    parameters: &'static str,
}

const SPECS: &[Spec] = &[
    Spec {
        name: "read",
        signature: "read(path, offset?, limit?)",
        description: "Return the contents of a text file. offset is a 1-based starting line, limit is a line count.",
        parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"offset":{"type":"integer","description":"1-based first line"},"limit":{"type":"integer","description":"Maximum number of lines"}},"required":["path"]}"#,
    },
    Spec {
        name: "write",
        signature: "write(path, content)",
        description: "Create or overwrite a file with the given content. Parent directories are created.",
        parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"content":{"type":"string","description":"Full file content"}},"required":["path","content"]}"#,
    },
    Spec {
        name: "edit",
        signature: "edit(path, old, new, replace_all?)",
        description: "Replace exact text in a file. Fails if old is absent or matches more than once unless replace_all is true.",
        parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"old":{"type":"string","description":"Exact text to replace"},"new":{"type":"string","description":"Replacement text"},"replace_all":{"type":"boolean","description":"Replace every occurrence"}},"required":["path","old","new"]}"#,
    },
    Spec {
        name: "bash",
        signature: "bash(command)",
        description: "Run a shell command in the working directory. Returns exit code, stdout, and stderr. Times out after 120 seconds.",
        parameters: r#"{"type":"object","properties":{"command":{"type":"string","description":"Command passed to sh -c"}},"required":["command"]}"#,
    },
];

static DEFINITIONS: OnceLock<Value> = OnceLock::new();

pub fn definitions() -> &'static Value {
    DEFINITIONS.get_or_init(|| {
        Value::Array(
            SPECS
                .iter()
                .map(|spec| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": spec.name,
                            "description": spec.description,
                            "parameters": serde_json::from_str::<Value>(spec.parameters)
                                .expect("tool schema"),
                        }
                    })
                })
                .collect(),
        )
    })
}

pub fn system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut out = format!("Working directory: {cwd}\n\nTools:\n");
    for spec in SPECS {
        out.push_str(&format!("- {} — {}\n", spec.signature, spec.description));
    }
    out
}

pub fn execute(name: &str, args: &str) -> String {
    let args: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!("error: invalid arguments: {e}"),
    };
    match name {
        "read" => read(&args),
        "write" => write(&args),
        "edit" => edit(&args),
        "bash" => bash(&args),
        other => format!("error: unknown tool: {other}"),
    }
}

fn read(args: &Value) -> String {
    let Some(path) = args["path"].as_str() else {
        return "error: missing \"path\"".into();
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return format!("error: read {path}: {e}"),
    };
    let text = String::from_utf8_lossy(&bytes);
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().map(|n| n as usize);

    let mut out = String::new();
    let mut taken = 0usize;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if number < offset {
            continue;
        }
        if let Some(limit) = limit {
            if taken >= limit {
                break;
            }
        }
        out.push_str(line);
        out.push('\n');
        taken += 1;
    }
    truncate(out, 50_000)
}

fn write(args: &Value) -> String {
    let Some(path) = args["path"].as_str() else {
        return "error: missing \"path\"".into();
    };
    let Some(content) = args["content"].as_str() else {
        return "error: missing \"content\"".into();
    };
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("error: create {}: {e}", parent.display());
            }
        }
    }
    match std::fs::write(path, content) {
        Ok(()) => format!("wrote {} bytes to {path}", content.len()),
        Err(e) => format!("error: write {path}: {e}"),
    }
}

fn edit(args: &Value) -> String {
    let Some(path) = args["path"].as_str() else {
        return "error: missing \"path\"".into();
    };
    let Some(old) = args["old"].as_str() else {
        return "error: missing \"old\"".into();
    };
    let Some(new) = args["new"].as_str() else {
        return "error: missing \"new\"".into();
    };
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return format!("error: read {path}: {e}"),
    };
    let count = text.matches(old).count();
    if count == 0 {
        return format!("error: edit {path}: text not found");
    }
    if count > 1 && !replace_all {
        return format!(
            "error: edit {path}: {count} matches; add more context or set replace_all"
        );
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    match std::fs::write(path, updated) {
        Ok(()) => format!("edited {path} ({count} replacement{})", plural(count)),
        Err(e) => format!("error: write {path}: {e}"),
    }
}

fn bash(args: &Value) -> String {
    let Some(command) = args["command"].as_str() else {
        return "error: missing \"command\"".into();
    };

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: spawn: {e}"),
    };

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let out_thread = thread::spawn(move || read_all(stdout));
    let err_thread = thread::spawn(move || read_all(stderr));

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(120) {
                    let _ = child.kill();
                    timed_out = true;
                    match child.wait() {
                        Ok(status) => break status,
                        Err(e) => return format!("error: wait: {e}"),
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                return format!("error: wait: {e}");
            }
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();

    let mut out = format!(
        "exit: {}\n",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into())
    );
    if !stdout.is_empty() {
        out.push_str(&stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str(&stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    if timed_out {
        out.push_str("timed out after 120s\n");
    }
    truncate(out, 30_000)
}

fn read_all(mut reader: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = reader.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn truncate(text: String, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text;
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text.chars().skip(total - max / 3).collect();
    format!("{head}\n...[truncated]...\n{tail}")
}
