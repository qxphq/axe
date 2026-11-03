use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http::Cancel;

struct Spec {
    name: &'static str,
    signature: &'static str,
    description: &'static str,
    parameters: &'static str,
}

const READ: Spec = Spec {
    name: "read",
    signature: "read(path, offset?, limit?)",
    description: "Return the contents of a text file. offset is a 1-based starting line, limit is a line count.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"offset":{"type":"integer","description":"1-based first line"},"limit":{"type":"integer","description":"Maximum number of lines"}},"required":["path"]}"#,
};

const WRITE: Spec = Spec {
    name: "write",
    signature: "write(path, content)",
    description: "Create or overwrite a file with the given content. Parent directories are created.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"content":{"type":"string","description":"Full file content"}},"required":["path","content"]}"#,
};

const EDIT: Spec = Spec {
    name: "edit",
    signature: "edit(path, old, new, replace_all?)",
    description: "Replace exact text in a file. Fails if old is absent or matches more than once unless replace_all is true.",
    parameters: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path relative to the working directory"},"old":{"type":"string","description":"Exact text to replace"},"new":{"type":"string","description":"Replacement text"},"replace_all":{"type":"boolean","description":"Replace every occurrence"}},"required":["path","old","new"]}"#,
};

const BASH: Spec = Spec {
    name: "bash",
    signature: "bash(command)",
    description: "Run a shell command in the working directory. Each call already starts in the working directory and no state persists between calls, so do not prefix commands with cd. Returns exit code, stdout, and stderr. Times out after 120 seconds.",
    parameters: r#"{"type":"object","properties":{"command":{"type":"string","description":"Command passed to sh -c"}},"required":["command"]}"#,
};

/// Explore mode's `bash`: same call, but only the commands below run. The
/// restriction is part of the description because the description is the
/// prompt — a first-token check cannot be expressed as an absent tool.
const BASH_READ_ONLY: Spec = Spec {
    name: "bash",
    signature: "bash(command)",
    description: "Run a read-only shell command in the working directory. Only these run: cat, cut, date, df, diff, du, fd, file, find, grep, head, id, jq, ls, ps, pwd, rg, sort, stat, tail, tr, tree, uname, uniq, wc, which, and git with one of blame, describe, diff, grep, log, ls-files, rev-parse, show, status. Anything else is refused. Each call already starts in the working directory and no state persists between calls, so do not prefix commands with cd. Returns exit code, stdout, and stderr. Times out after 120 seconds.",
    parameters: r#"{"type":"object","properties":{"command":{"type":"string","description":"Command passed to sh -c"}},"required":["command"]}"#,
};

/// What `bash` may run in explore mode: commands that only read. A first-token
/// check, not a sandbox — it keeps a model that was told to explore from
/// rewriting the tree, and that is all it claims to do.
const READ_ONLY: &[&str] = &[
    "cat", "cut", "date", "df", "diff", "du", "fd", "file", "find", "grep", "head", "id", "jq",
    "ls", "ps", "pwd", "rg", "sort", "stat", "tail", "tr", "tree", "uname", "uniq", "wc", "which",
];

/// `git` reads through its subcommand, so the first token alone says nothing.
const READ_ONLY_GIT: &[&str] = &[
    "blame", "describe", "diff", "grep", "log", "ls-files", "rev-parse", "show", "status",
];

const SPECS: &[Spec] = &[READ, WRITE, EDIT, BASH];
const SPECS_READ_ONLY: &[Spec] = &[READ, BASH_READ_ONLY];

fn specs(read_only: bool) -> &'static [Spec] {
    if read_only {
        SPECS_READ_ONLY
    } else {
        SPECS
    }
}

static DEFINITIONS: OnceLock<Value> = OnceLock::new();
static DEFINITIONS_READ_ONLY: OnceLock<Value> = OnceLock::new();

pub fn definitions(read_only: bool) -> &'static Value {
    let slot = if read_only {
        &DEFINITIONS_READ_ONLY
    } else {
        &DEFINITIONS
    };
    slot.get_or_init(|| {
        Value::Array(
            specs(read_only)
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

pub fn system_prompt(read_only: bool) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut out = format!("Working directory: {cwd}\n\nTools:\n");
    for spec in specs(read_only) {
        out.push_str(&format!("- {} — {}\n", spec.signature, spec.description));
    }
    out
}

pub fn execute(name: &str, args: &str, cancel: &Cancel, read_only: bool) -> String {
    let args: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return format!("error: invalid arguments: {e}"),
    };
    match name {
        "read" => read(&args),
        "bash" if read_only => read_only_bash(&args, cancel),
        "bash" => bash(&args, cancel),
        // Not in the schema explore mode sends; a call is the model recalling
        // a tool it no longer has, and the result says why.
        "write" | "edit" if read_only => format!("error: explore mode: {name} is not available"),
        "write" => write(&args),
        "edit" => edit(&args),
        other => format!("error: unknown tool: {other}"),
    }
}

/// Refuse the command before it spawns: the check is on the call's first word,
/// so nothing is left running when it fails.
fn read_only_bash(args: &Value, cancel: &Cancel) -> String {
    let Some(command) = args["command"].as_str() else {
        return "error: missing \"command\"".into();
    };
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return "error: missing \"command\"".into();
    };
    if first == "git" {
        let sub = words.next().unwrap_or("");
        if READ_ONLY_GIT.contains(&sub) {
            return bash(args, cancel);
        }
        return format!(
            "error: explore mode: git {sub} is not read-only; allowed: git {}",
            READ_ONLY_GIT.join(", git ")
        );
    }
    if READ_ONLY.contains(&first) {
        return bash(args, cancel);
    }
    format!(
        "error: explore mode: {first} is not read-only; allowed: {}",
        READ_ONLY.join(", ")
    )
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

fn bash(args: &Value, cancel: &Cancel) -> String {
    let Some(command) = args["command"].as_str() else {
        return "error: missing \"command\"".into();
    };

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so an interrupt kills the whole pipeline.
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: spawn: {e}"),
    };
    let pid = child.id() as i32;

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let out_thread = thread::spawn(move || read_all(stdout));
    let err_thread = thread::spawn(move || read_all(stderr));

    let start = Instant::now();
    let mut timed_out = false;
    let mut interrupted = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if cancel.cancelled() && !interrupted {
                    interrupted = true;
                    // `--` is not optional: without it `kill` reads the
                    // negative pid as an option and signals nothing.
                    kill("-TERM", pid);
                    // Grace period, then the whole group gets SIGKILL.
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if child.try_wait().ok().flatten().is_none() {
                        kill("-KILL", pid);
                    }
                    match child.wait() {
                        Ok(status) => break status,
                        Err(e) => return format!("error: wait: {e}"),
                    }
                }
                if !timed_out && start.elapsed() > Duration::from_secs(120) {
                    let _ = child.kill();
                    timed_out = true;
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
    } else if interrupted {
        out.push_str("interrupted by user (ESC); the command was killed\n");
    }
    truncate(out, 30_000)
}

/// Signal the whole process group: a pipeline must not outlive its `sh`.
/// `--` is not optional and the error output is dropped: by the time the group
/// is gone, `kill` reporting that is noise on the user's terminal.
fn kill(signal: &str, pid: i32) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
