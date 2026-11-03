use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Set by the ESC watcher when the user asks to interrupt a running command.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Consume the interrupt request, if any. The agent loop ends the turn on it.
pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Watches /dev/tty for ESC while a command runs. First ESC arms the interrupt
/// ("press again"), the second raises `INTERRUPTED`. Restores tty settings on
/// drop; reads are made non-blocking through termios (`min 0 time 1`) so the
/// watcher exits within ~100 ms instead of swallowing the next REPL line.
struct EscWatch {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    saved: Option<String>,
}

impl EscWatch {
    fn start() -> EscWatch {
        let mut this = EscWatch {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            saved: stty(&["-g"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        };
        if this.saved.is_none()
            || stty(&["-icanon", "-echo", "min", "0", "time", "1"]).is_none()
        {
            return this;
        }
        let Ok(mut tty) = std::fs::OpenOptions::new().read(true).open("/dev/tty") else {
            this.restore();
            return this;
        };

        let stop = this.stop.clone();
        this.thread = Some(thread::spawn(move || {
            let mut buf = [0u8; 64];
            let mut armed = false;
            while !stop.load(Ordering::SeqCst) {
                let n = match tty.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => n,
                    Err(_) => break,
                };
                for (i, byte) in buf[..n].iter().enumerate() {
                    if *byte != 0x1b {
                        continue;
                    }
                    // ESC followed by more bytes is an escape sequence (arrows).
                    if matches!(buf.get(i + 1), Some(b'[') | Some(b'O')) {
                        break;
                    }
                    if armed {
                        INTERRUPTED.store(true, Ordering::SeqCst);
                        eprintln!("\nnano: interrupting…");
                        return;
                    }
                    armed = true;
                    eprintln!("\nnano: press ESC again to interrupt the running command");
                }
            }
        }));
        this
    }

    fn restore(&mut self) {
        if let Some(saved) = self.saved.take() {
            stty(&[&saved]);
        }
    }
}

/// Put the tty into immediate, unechoed input mode for the REPL. Returns the
/// previous settings, to be handed back to `restore_tty`; `None` if `stty`
/// is unavailable, in which case the caller reads whole lines as usual.
pub fn raw_input() -> Option<String> {
    let saved = stty(&["-g"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    stty(&["-icanon", "-echo", "-isig", "min", "1", "time", "0"])?;
    Some(saved)
}

/// Restore settings captured by `raw_input`.
pub fn restore_tty(saved: &str) {
    stty(&[saved]);
}

/// Panic-hook variant: re-enable canonical echo from the tty's own defaults,
/// since the saved settings string is not reachable there.
pub fn restore_tty_now() -> Option<String> {
    stty(&["echo", "icanon", "isig"])
}

impl Drop for EscWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Only restore after the watcher is gone, so a pending read cannot hang
        // on blocking termios settings.
        self.restore();
    }
}

fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty")
        .arg("-F")
        .arg("/dev/tty")
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

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
        description: "Run a shell command in the working directory. Each call already starts in the working directory and no state persists between calls, so do not prefix commands with cd. Returns exit code, stdout, and stderr. Times out after 120 seconds.",
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
        // Own process group, so an interrupt kills the whole pipeline.
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("error: spawn: {e}"),
    };
    let pid = child.id() as i32;
    INTERRUPTED.store(false, Ordering::SeqCst);
    let watch = EscWatch::start();

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
                if INTERRUPTED.load(Ordering::SeqCst) && !interrupted {
                    interrupted = true;
                    INTERRUPTED.store(false, Ordering::SeqCst);
                    let _ = Command::new("kill")
                        .arg("-TERM")
                        .arg(format!("-{pid}"))
                        .status();
                    // Grace period, then the whole group gets SIGKILL.
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline && child.try_wait().ok().flatten().is_none() {
                        thread::sleep(Duration::from_millis(20));
                    }
                    let _ = Command::new("kill")
                        .arg("-KILL")
                        .arg(format!("-{pid}"))
                        .status();
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
    drop(watch);

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
