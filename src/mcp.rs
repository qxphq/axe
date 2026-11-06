//! MCP servers: the two config files, the programs axe starts from them, and
//! the tools they add. stdio only — a server is a program, not a URL — and the
//! catalogue they build is fixed at startup, so the cached prefix never moves.
//!
//! The global file is `~/.config/axe/mcp.json`; a project's is `.mcp.json` at
//! or above the working directory, never past its repository root. A project
//! file is code a repository shipped, so it runs only after one approval per
//! file, remembered by content hash in the project's state directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::Cancel;
use crate::state;
use crate::text::{clip, plural, strip_control, truncate};

/// The revision axe speaks and the versions it understands. A server answers
/// with the one it will use; axe does not check it, the shapes it needs are in
/// both.
const PROTOCOL: &str = "2025-06-18";
/// A server that has not answered `initialize` by then is a failure, not a wait.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Seconds a `tools/call` may take unless the entry says otherwise.
const CALL_TIMEOUT: u64 = 60;
/// One JSON-RPC message. A line longer than this is a protocol error.
const LINE_CAP: usize = 4 << 20;
/// The tail of a server's stderr kept for an error message.
const STDERR_CAP: usize = 64 * 1024;
/// Tools one server may contribute, and the schema bytes it may add to the
/// request: a server with hundreds of tools would flood the prefix.
const TOOLS_PER_SERVER: usize = 128;
const SCHEMA_BYTES: usize = 256 * 1024;
/// The model-facing result cap, like `bash`'s.
const RESULT_CAP: usize = 30_000;
/// OpenAI's tool-name limit.
const NAME_CAP: usize = 64;
const DESCRIPTION_CAP: usize = 1024;

pub fn global_path() -> PathBuf {
    crate::config::path().with_file_name("mcp.json")
}

/// `<repo>/.mcp.json`: the nearest one at or above `cwd`, and never above the
/// repository root — a checkout has one project file, not its parent's.
fn project_file(cwd: &Path) -> Option<PathBuf> {
    let mut at = Some(cwd);
    while let Some(dir) = at {
        let file = dir.join(".mcp.json");
        if file.is_file() {
            return Some(file);
        }
        if dir.join(".git").exists() {
            return None;
        }
        at = dir.parent();
    }
    None
}

/// Where `/mcp add --project` writes: the existing project file, else the
/// repository root, else the working directory.
fn project_target(cwd: &Path) -> PathBuf {
    if let Some(file) = project_file(cwd) {
        return file;
    }
    let mut at = Some(cwd);
    while let Some(dir) = at {
        if dir.join(".git").exists() {
            return dir.join(".mcp.json");
        }
        at = dir.parent();
    }
    cwd.join(".mcp.json")
}

/// One entry of `mcpServers`, as both files spell it.
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    command: String,
    args: Vec<String>,
    /// Variables from axe's environment handed to the server. Names only: a
    /// value in a file is a secret in a file.
    env: Vec<String>,
    enabled: bool,
    timeout: u64,
    /// The file that defines it, for `/mcp` and for `remove`.
    source: PathBuf,
    /// A repository supplied it, so it needed the approval.
    project: bool,
    /// The entry carries no command: it only turns a global server on or off.
    toggle_only: bool,
}

impl Entry {
    fn describe(&self) -> String {
        if self.toggle_only {
            return if self.enabled {
                "turns the global server on".into()
            } else {
                "turns the global server off".into()
            };
        }
        let mut line = self.command.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        if !self.env.is_empty() {
            line.push_str(&format!("  env: {}", self.env.join(", ")));
        }
        line
    }
}

/// Server names become tool names, so the charset is the one both can use.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// `~` and `~/…` name the home directory, as in a shell: an argument a user
/// wrote by hand is read the way the user meant it.
fn expand_home(raw: &str) -> String {
    if raw == "~" {
        return home().display().to_string();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home().join(rest).display().to_string(),
        None => raw.to_string(),
    }
}

fn valid_var(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// One file's entries. Unknown keys are errors: a typo, or a field from
/// another client that axe would otherwise ignore and half-run.
fn parse_servers(path: &Path, project: bool) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut errors = Vec::new();
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (entries, errors),
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            return (entries, errors);
        }
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(e) => {
            errors.push(format!("{}: {e}", path.display()));
            return (entries, errors);
        }
    };
    let Some(servers) = parsed["mcpServers"].as_object() else {
        if parsed.get("mcpServers").is_some() {
            errors.push(format!(
                "{}: \"mcpServers\" must be an object",
                path.display()
            ));
        }
        return (entries, errors);
    };
    for (name, value) in servers {
        match parse_entry(name, value, path, project) {
            Ok(entry) => entries.push(entry),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    (entries, errors)
}

fn parse_entry(name: &str, value: &Value, path: &Path, project: bool) -> Result<Entry, String> {
    if !valid_name(name) {
        return Err(format!(
            "server \"{name}\": names use letters, digits, \"_\" and \"-\" (32 at most)"
        ));
    }
    let Some(object) = value.as_object() else {
        return Err(format!("server \"{name}\": must be an object"));
    };
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "command" | "args" | "env" | "enabled" | "timeout" | "type" | "url"
        ) {
            return Err(format!(
                "server \"{name}\": unknown key \"{key}\" (axe takes command, args, env, \
                 enabled, timeout)"
            ));
        }
    }
    if let Some(url) = object.get("url") {
        if !url.is_null() {
            return Err(format!(
                "server \"{name}\": remote servers are not supported yet; axe runs stdio \
                 servers only"
            ));
        }
    }
    if let Some(kind) = object.get("type").and_then(Value::as_str) {
        if kind != "stdio" {
            return Err(format!(
                "server \"{name}\": type \"{kind}\" is not supported; axe runs stdio servers only"
            ));
        }
    }
    let enabled = match object.get("enabled") {
        None => true,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("server \"{name}\": \"enabled\" must be true or false"))?,
    };
    let timeout = match object.get("timeout") {
        None => CALL_TIMEOUT,
        Some(value) => value
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| format!("server \"{name}\": \"timeout\" must be a positive number"))?,
    };
    let env = parse_env(name, object.get("env"))?;
    let command = match object.get("command") {
        Some(value) => value
            .as_str()
            .filter(|command| !command.is_empty())
            .ok_or_else(|| format!("server \"{name}\": \"command\" must be a string"))?
            .to_string(),
        None => {
            // A project entry with no command can only switch a global server:
            // a repository may narrow what runs, never define it.
            if !project {
                return Err(format!("server \"{name}\": missing \"command\""));
            }
            if object.get("enabled").is_none() {
                return Err(format!(
                    "server \"{name}\": needs \"command\", or \"enabled\" to switch a \
                     global server"
                ));
            }
            if object.contains_key("args")
                || object.contains_key("env")
                || object.contains_key("timeout")
            {
                return Err(format!(
                    "server \"{name}\": an entry without \"command\" may only set \"enabled\""
                ));
            }
            return Ok(Entry {
                name: name.to_string(),
                command: String::new(),
                args: Vec::new(),
                env,
                enabled,
                timeout,
                source: path.to_path_buf(),
                project,
                toggle_only: true,
            });
        }
    };
    let args = match object.get("args") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| format!("server \"{name}\": \"args\" must be an array of strings"))?
            .iter()
            .map(|arg| {
                arg.as_str().map(expand_home).ok_or_else(|| {
                    format!("server \"{name}\": \"args\" must be an array of strings")
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(Entry {
        name: name.to_string(),
        command: expand_home(&command),
        args,
        env,
        enabled,
        timeout,
        source: path.to_path_buf(),
        project,
        toggle_only: false,
    })
}

/// `env` is a list of names. An object here is another client's shape, and
/// silently ignoring it would run the server without the variables it asked
/// for, so it is the one error worth spelling out.
fn parse_env(name: &str, value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut names = Vec::new();
            for item in items {
                let raw = item.as_str().ok_or_else(|| {
                    format!("server \"{name}\": \"env\" must be a list of variable names")
                })?;
                if !valid_var(raw) {
                    return Err(format!(
                        "server \"{name}\": env \"{raw}\" is not a variable name"
                    ));
                }
                names.push(raw.to_string());
            }
            Ok(names)
        }
        Some(_) => Err(format!(
            "server \"{name}\": \"env\" is a list of variable names, not a map; axe never \
             reads a value from a config file"
        )),
    }
}

/// Global entries, then the project's on top. A project entry replaces the
/// global one by name; a toggle-only entry changes its `enabled` and nothing
/// else.
fn merge(global: Vec<Entry>, project: Vec<Entry>, errors: &mut Vec<String>) -> Vec<Entry> {
    let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();
    for entry in global {
        by_name.insert(entry.name.clone(), entry);
    }
    for entry in project {
        if entry.toggle_only {
            match by_name.get_mut(&entry.name) {
                Some(base) => base.enabled = entry.enabled,
                None => errors.push(format!(
                    "{}: server \"{}\" turns on nothing: no global server has that name",
                    entry.source.display(),
                    entry.name
                )),
            }
            continue;
        }
        by_name.insert(entry.name.clone(), entry);
    }
    by_name.into_values().collect()
}

// ---- the approval a project file needs -------------------------------------

fn trust_file(cwd: &Path) -> PathBuf {
    state::project(cwd).join("mcp-trust.json")
}

fn hash_of(bytes: &[u8]) -> String {
    format!("{:016x}", state::fnv1a(bytes))
}

fn read_trust(cwd: &Path) -> BTreeMap<String, String> {
    let Ok(raw) = fs::read_to_string(trust_file(cwd)) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return BTreeMap::new();
    };
    map.into_iter()
        .filter_map(|(path, hash)| Some((path, hash.as_str()?.to_string())))
        .collect()
}

fn approved(cwd: &Path, file: &Path, hash: &str) -> bool {
    read_trust(cwd)
        .get(&file.display().to_string())
        .is_some_and(|remembered| remembered == hash)
}

fn remember(cwd: &Path, file: &Path, hash: &str) {
    let mut trust = read_trust(cwd);
    trust.insert(file.display().to_string(), hash.to_string());
    let dir = state::project(cwd);
    if let Err(e) = state::private_dir(&dir) {
        eprintln!("axe: mcp trust: {e}");
        return;
    }
    let value: BTreeMap<&String, &String> = trust.iter().collect();
    let text = match serde_json::to_string_pretty(&value) {
        Ok(text) => format!("{text}\n"),
        Err(e) => {
            eprintln!("axe: mcp trust: {e}");
            return;
        }
    };
    if let Err(e) = state::write_private(&trust_file(cwd), &text) {
        eprintln!("axe: mcp trust: {e}");
    }
}

fn forget_trust(cwd: &Path) -> String {
    let file = trust_file(cwd);
    match fs::remove_file(&file) {
        Ok(()) => "mcp: forgotten; the next start asks again, running servers stay".into(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => "mcp: nothing remembered".into(),
        Err(e) => format!("error: mcp: {}: {e}", file.display()),
    }
}

/// The one question: what the repository wants to run, and which of the
/// environment it wants to see.
fn ask(file: &Path, entries: &[Entry]) -> bool {
    let mut text = format!("mcp {} defines:\n", file.display());
    for entry in entries {
        text.push_str(&format!(
            "  {}  {}\n",
            entry.name,
            strip_control(&entry.describe())
        ));
    }
    text.push_str("run them? [y/N] ");
    print!("{text}");
    io::stdout().flush().ok();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(line.trim(), "y" | "Y" | "yes"),
    }
}

// ---- the processes ---------------------------------------------------------

/// One tool a server listed, before it is given a name in axe's catalogue.
struct Listed {
    original: String,
    description: String,
    schema: Value,
}

struct Tool {
    /// The name the model calls, `mcp__<server>__<tool>`.
    exposed: String,
    /// The name the server knows.
    original: String,
    definition: Value,
}

struct Server {
    name: String,
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    /// Replies by id, filled by the reader thread.
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    next: u64,
    timeout: Duration,
    listed: Vec<Listed>,
    tools: Vec<Tool>,
    dead: Arc<AtomicBool>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The process group is gone. A reaped pid may be reused, so this is what
    /// keeps `Drop` from signalling a group that is not ours any more.
    stopped: bool,
}

impl Server {
    fn open(entry: &Entry, explore: bool) -> Result<Server, String> {
        let program = match entry.command.contains('/') {
            true => PathBuf::from(&entry.command),
            false => crate::sandbox::resolve_program(&entry.command)
                .ok_or_else(|| format!("\"{}\" is not on PATH", entry.command))?,
        };
        let mut env = Vec::new();
        for name in &entry.env {
            match std::env::var(name) {
                Ok(value) => env.push((name.clone(), value)),
                Err(_) => return Err(format!("{name} is not set")),
            }
        }
        let mut cmd = crate::sandbox::server(&program, &entry.args, &env, explore)?;
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so a restart takes its runtime with it.
            .process_group(0)
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", program.display()))?;
        let pid = child.id() as i32;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let pending: Arc<Mutex<HashMap<u64, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(stdout) = stdout {
            let pending = pending.clone();
            let dead = dead.clone();
            thread::spawn(move || read_replies(stdout, pending, dead));
        }
        if let Some(pipe) = stderr_pipe {
            let tail = stderr.clone();
            thread::spawn(move || read_stderr(pipe, tail));
        }
        let mut server = Server {
            name: entry.name.clone(),
            child,
            pid,
            stdin,
            pending,
            next: 1,
            timeout: Duration::from_secs(entry.timeout),
            listed: Vec::new(),
            tools: Vec::new(),
            dead,
            stderr,
            stopped: false,
        };
        server.initialize()?;
        server.list_tools()?;
        Ok(server)
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "axe", "version": env!("CARGO_PKG_VERSION") },
            }),
            None,
            INIT_TIMEOUT,
        )?;
        self.notify("notifications/initialized", json!({}))
    }

    fn list_tools(&mut self) -> Result<(), String> {
        let mut cursor: Option<String> = None;
        let mut bytes = 0usize;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.request("tools/list", params, None, INIT_TIMEOUT)?;
            for tool in result["tools"].as_array().cloned().unwrap_or_default() {
                if self.listed.len() >= TOOLS_PER_SERVER {
                    return Ok(());
                }
                let Some(name) = tool["name"].as_str().filter(|name| !name.is_empty()) else {
                    continue;
                };
                let schema = match tool["inputSchema"].is_object() {
                    true => tool["inputSchema"].clone(),
                    false => json!({ "type": "object" }),
                };
                bytes += serde_json::to_string(&schema).map(|s| s.len()).unwrap_or(0);
                if bytes > SCHEMA_BYTES {
                    return Ok(());
                }
                let description = tool["description"]
                    .as_str()
                    .map(|text| format!("{} (MCP server \"{}\")", strip_control(text), self.name))
                    .unwrap_or_else(|| format!("MCP server \"{}\" tool \"{name}\"", self.name));
                self.listed.push(Listed {
                    original: name.to_string(),
                    description: clip(&description, DESCRIPTION_CAP),
                    schema,
                });
            }
            cursor = result["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                return Ok(());
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn write(&mut self, message: &Value) -> Result<(), String> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err("the server's input is closed".into());
        };
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("write: {e}"))
    }

    /// One request and its reply. `cancel` is checked every 20 ms, like a
    /// command's: ESC ends the wait, tells the server, and leaves it running.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: Option<&Cancel>,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(self.exited());
        }
        let id = self.next;
        self.next += 1;
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write(&message) {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&id);
            }
            return Err(e);
        }
        let start = Instant::now();
        let mut cancelled = false;
        let reply = loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(reply) => break Some(reply),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_some_and(Cancel::cancelled) {
                        cancelled = true;
                        break None;
                    }
                    if start.elapsed() > timeout {
                        break None;
                    }
                }
            }
        };
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&id);
        }
        let Some(reply) = reply else {
            if cancelled || cancel.is_some_and(Cancel::cancelled) {
                let _ = self.notify(
                    "notifications/cancelled",
                    json!({ "requestId": id, "reason": "interrupted by user" }),
                );
                return Err("interrupted by user (ESC); the call was not run".into());
            }
            if self.dead.load(Ordering::SeqCst) {
                return Err(self.exited());
            }
            let _ = self.notify(
                "notifications/cancelled",
                json!({ "requestId": id, "reason": "timeout" }),
            );
            return Err(format!("timed out after {}s", timeout.as_secs()));
        };
        if reply.is_null() {
            return Err(self.exited());
        }
        if let Some(error) = reply.get("error") {
            return Err(format!("{method}: {}", explain(error)));
        }
        Ok(reply["result"].clone())
    }

    fn call(&mut self, tool: &str, args: &Value, cancel: &Cancel) -> String {
        let arguments = match args.is_object() {
            true => args.clone(),
            false => json!({}),
        };
        match self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
            Some(cancel),
            self.timeout,
        ) {
            Ok(result) => render(&result),
            Err(e) => format!("error: mcp {}: {e}", self.name),
        }
    }

    /// What to say when the server is gone: its own last words, if it had any.
    fn exited(&self) -> String {
        let tail = self
            .stderr
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .unwrap_or_default();
        let mut out = "the server exited".to_string();
        if !tail.is_empty() {
            out.push_str(": ");
            out.push_str(&clip(&strip_control(&tail), 800));
        }
        out
    }

    /// MCP's shutdown: close the input, give it two seconds, then take the
    /// process group. A runtime the server started goes with it.
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{}", self.pid))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg("--")
            .arg(format!("-{}", self.pid))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.wait();
    }
}

/// Replies, by id. Notifications are dropped: axe advertises no capability
/// that would make one its business, and a list change is not followed — the
/// catalogue is fixed for the session.
fn read_replies(
    stdout: impl Read,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    dead: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        // `take` bounds one read: a server that never sends a newline must not
        // grow this buffer without limit.
        let read = (&mut reader)
            .take((LINE_CAP + 1) as u64)
            .read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() > LINE_CAP => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let Ok(message) = serde_json::from_str::<Value>(text.trim()) else {
            continue;
        };
        let Some(id) = message["id"].as_u64() else {
            continue;
        };
        if let Ok(mut map) = pending.lock() {
            if let Some(tx) = map.remove(&id) {
                let _ = tx.send(message);
            }
        }
    }
    dead.store(true, Ordering::SeqCst);
    // Nothing will answer these now.
    if let Ok(mut map) = pending.lock() {
        for (_, tx) in map.drain() {
            let _ = tx.send(Value::Null);
        }
    }
}

/// The tail of stderr, for the one line that says why a server failed.
fn read_stderr(pipe: impl Read, tail: Arc<Mutex<Vec<u8>>>) {
    let mut reader = BufReader::new(pipe);
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.extend_from_slice(&buf[..read]);
                    if tail.len() > STDERR_CAP {
                        let cut = tail.len() - STDERR_CAP;
                        tail.drain(..cut);
                    }
                }
            }
        }
    }
}

fn explain(error: &Value) -> String {
    let message = error["message"].as_str().unwrap_or("request failed");
    match error["code"].as_i64() {
        Some(code) => format!("{message} ({code})"),
        None => message.to_string(),
    }
}

/// A tool result as the model reads it: text joined, what is not text named,
/// and `isError` the one thing that makes it an error.
fn render(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result["content"].as_array().cloned().unwrap_or_default() {
        match item["type"].as_str() {
            Some("text") => {
                if let Some(text) = item["text"].as_str() {
                    parts.push(strip_control(text));
                }
            }
            Some("image") => parts.push("[image omitted]".to_string()),
            Some("audio") => parts.push("[audio omitted]".to_string()),
            Some(other) => parts.push(format!("[{other} omitted]")),
            None => {}
        }
    }
    let error = result["isError"].as_bool().unwrap_or(false);
    let mut text = parts.join("\n").trim().to_string();
    if text.is_empty() {
        text = if error {
            "the server reported an error".to_string()
        } else {
            "done".to_string()
        };
    }
    // The transcript marks a failed call by the prefix the harness reads.
    if error {
        text = format!("error: {text}");
    }
    if text.chars().count() > RESULT_CAP {
        let full = save(&text);
        let text = truncate(text, RESULT_CAP);
        text_with_path(text, full)
    } else {
        text
    }
}

fn text_with_path(mut text: String, full: io::Result<PathBuf>) -> String {
    if let Ok(path) = full {
        text.push_str(&format!("\n[full output: {}]", path.display()));
    }
    text
}

/// The whole text of a cut result, owner-only, in the temporary directory. It
/// stays there: the model may read it, and the OS cleans it up.
fn save(text: &str) -> io::Result<PathBuf> {
    for n in 0..16 {
        let path = std::env::temp_dir().join(format!("axe-mcp-{}-{n}.txt", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name"))
}

// ---- what the session holds -------------------------------------------------

struct Stamp {
    len: u64,
    modified: Option<u64>,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some(Stamp {
        len: meta.len(),
        modified: meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    })
}

#[derive(Default)]
struct Registry {
    entries: Vec<Entry>,
    /// Indexed like `entries`: a server per enabled entry, or why it failed.
    running: Vec<Option<Server>>,
    failures: Vec<Option<String>>,
    errors: Vec<String>,
    cwd: Option<PathBuf>,
    project: Option<PathBuf>,
    trusted: bool,
    stamps: Vec<(PathBuf, Option<Stamp>)>,
    explore: bool,
}

impl Registry {
    fn index(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|entry| entry.name == name)
    }

    /// The tool name the model calls, resolved to the server that owns it.
    fn find(&self, exposed: &str) -> Option<(usize, usize)> {
        for (at, server) in self.running.iter().enumerate() {
            let Some(server) = server else { continue };
            if let Some(tool) = server.tools.iter().position(|tool| tool.exposed == exposed) {
                return Some((at, tool));
            }
        }
        None
    }
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Load both files, ask about a project's if it is new, start the servers, and
/// keep them. The notices go to the transcript before the first prompt.
pub fn start(cwd: Option<&Path>, interactive: bool, explore: bool) -> Vec<String> {
    let global = global_path();
    let project = cwd.and_then(project_file);
    let mut errors = Vec::new();
    let global_entries = {
        let (entries, mut problems) = parse_servers(&global, false);
        errors.append(&mut problems);
        entries
    };
    let (project_entries, mut problems) = match &project {
        Some(file) => parse_servers(file, true),
        None => (Vec::new(), Vec::new()),
    };
    errors.append(&mut problems);

    let mut trusted = project.is_none();
    if let Some(file) = &project {
        let bytes = fs::read(file).unwrap_or_default();
        let hash = hash_of(&bytes);
        trusted = cwd.is_some_and(|dir| approved(dir, file, &hash));
        // A file that only switches global servers runs nothing new: it needs
        // no approval, and its entries are applied either way.
        let runs = project_entries.iter().any(|entry| !entry.toggle_only);
        if !trusted && runs {
            if interactive && ask(file, &project_entries) {
                if let Some(dir) = cwd {
                    remember(dir, file, &hash);
                }
                trusted = true;
            }
        } else if !runs {
            trusted = true;
        }
    }

    let entries = merge(
        global_entries,
        if trusted { project_entries } else { Vec::new() },
        &mut errors,
    );
    let mut notices: Vec<String> = errors.iter().map(|e| format!("mcp: {e}")).collect();
    if project.is_some() && !trusted {
        notices.push(format!(
            "mcp: {} is not approved, so its servers are not running",
            project.as_deref().unwrap_or(Path::new("")).display()
        ));
    }

    let mut running = Vec::with_capacity(entries.len());
    let mut failures = Vec::with_capacity(entries.len());
    for entry in &entries {
        if !entry.enabled {
            running.push(None);
            failures.push(None);
            continue;
        }
        match Server::open(entry, explore) {
            Ok(server) => {
                notices.push(format!(
                    "mcp {}: {} tool{}{}",
                    entry.name,
                    server.listed.len(),
                    plural(server.listed.len()),
                    origin(entry)
                ));
                running.push(Some(server));
                failures.push(None);
            }
            Err(e) => {
                notices.push(format!("mcp {}: {}{}", entry.name, e, origin(entry)));
                running.push(None);
                failures.push(Some(e));
            }
        }
    }
    name_tools(&mut running);

    let stamps = vec![(global.clone(), stamp(&global))]
        .into_iter()
        .chain(project.clone().map(|file| (file.clone(), stamp(&file))))
        .collect();
    *lock() = Registry {
        entries,
        running,
        failures,
        errors,
        cwd: cwd.map(Path::to_path_buf),
        project,
        trusted,
        stamps,
        explore,
    };
    notices
}

fn origin(entry: &Entry) -> &'static str {
    if entry.project {
        " (project)"
    } else {
        " (global)"
    }
}

/// Tool names, globally: `mcp__<server>__<tool>`, and a hash suffix when the
/// name is too long or already taken, so two servers can never collide.
fn name_tools(running: &mut [Option<Server>]) {
    let mut taken: HashSet<String> = HashSet::new();
    for server in running.iter_mut().flatten() {
        let listed = std::mem::take(&mut server.listed);
        for item in listed {
            let exposed = tool_name(&server.name, &item.original, &taken);
            taken.insert(exposed.clone());
            let definition = json!({
                "type": "function",
                "function": {
                    "name": exposed,
                    "description": item.description,
                    "parameters": item.schema,
                }
            });
            server.tools.push(Tool {
                exposed,
                original: item.original,
                definition,
            });
        }
    }
}

fn tool_name(server: &str, tool: &str, taken: &HashSet<String>) -> String {
    let sanitized = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("mcp__{}__{}", sanitized(server), sanitized(tool));
    if name.len() <= NAME_CAP && !taken.contains(&name) {
        return name;
    }
    let hash = format!(
        "{:08x}",
        state::fnv1a(format!("{server}\0{tool}").as_bytes()) as u32
    );
    name.truncate(NAME_CAP - hash.len() - 1);
    name.push('_');
    name.push_str(&hash);
    name
}

/// Append to the tool array the request carries.
pub fn definitions() -> Vec<Value> {
    let reg = lock();
    reg.running
        .iter()
        .flatten()
        .flat_map(|server| server.tools.iter().map(|tool| tool.definition.clone()))
        .collect()
}

/// One `tools/call`, if the name is an MCP tool. The mode does not enter here:
/// the confinement was decided when the server was started.
pub fn call(name: &str, args: &Value, cancel: &Cancel) -> String {
    let mut reg = lock();
    let Some((server, tool)) = reg.find(name) else {
        return format!("error: unknown tool: {name}");
    };
    let Some(Some(server)) = reg.running.get_mut(server) else {
        return format!("error: mcp: {name} has no server");
    };
    let original = server.tools[tool].original.clone();
    server.call(&original, args, cancel)
}

/// A mode flip moves the confinement, and a running process cannot be narrowed
/// or widened: every server is restarted under the new rules. The tool names
/// stay what they were; a server that fails now only answers with errors.
pub fn restart(explore: bool) -> Vec<String> {
    let mut reg = lock();
    if reg.entries.is_empty() || reg.explore == explore {
        reg.explore = explore;
        return Vec::new();
    }
    reg.explore = explore;
    // The catalogue the model was given is the one that stays: a restart
    // reuses the names and definitions it saw, so a server that lists a
    // different set cannot move the prefix mid-session.
    let mut kept: HashMap<(String, String), (String, Value)> = HashMap::new();
    for (entry, server) in reg.entries.iter().zip(reg.running.iter()) {
        let Some(server) = server else { continue };
        for tool in &server.tools {
            kept.insert(
                (entry.name.clone(), tool.original.clone()),
                (tool.exposed.clone(), tool.definition.clone()),
            );
        }
    }
    for server in reg.running.iter_mut().flatten() {
        server.stop();
    }
    let mut notices = Vec::new();
    let entries = reg.entries.clone();
    for (at, entry) in entries.iter().enumerate() {
        if !entry.enabled {
            continue;
        }
        match Server::open(entry, explore) {
            Ok(mut server) => {
                let listed = std::mem::take(&mut server.listed);
                for item in listed {
                    let Some((exposed, definition)) =
                        kept.get(&(entry.name.clone(), item.original.clone()))
                    else {
                        continue;
                    };
                    server.tools.push(Tool {
                        exposed: exposed.clone(),
                        original: item.original,
                        definition: definition.clone(),
                    });
                }
                notices.push(format!(
                    "mcp {}: {} tool{}{}",
                    entry.name,
                    server.tools.len(),
                    plural(server.tools.len()),
                    origin(entry)
                ));
                reg.running[at] = Some(server);
                reg.failures[at] = None;
            }
            Err(e) => {
                notices.push(format!("mcp {}: {}{}", entry.name, e, origin(entry)));
                reg.running[at] = None;
                reg.failures[at] = Some(e);
            }
        }
    }
    notices
}

/// At exit: no server outlives the session that started it.
pub fn shutdown() {
    let reg = registry();
    let Ok(mut reg) = reg.lock() else { return };
    for server in reg.running.iter_mut().flatten() {
        server.stop();
    }
}

// ---- `/mcp` -----------------------------------------------------------------

/// `/mcp`, `/mcp <name>`, and the three edits. One entry point so the REPL's
/// dispatch stays a line.
pub fn dispatch(arg: &str) -> String {
    let arg = arg.trim();
    if arg.is_empty() {
        return listing();
    }
    let (verb, rest) = match arg.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (arg, ""),
    };
    match verb {
        "add" => add(rest),
        "remove" => remove(rest),
        "forget" => forget(),
        "list" => listing(),
        "help" => empty_state(),
        name => detail(name),
    }
}

fn listing() -> String {
    let reg = lock();
    if reg.entries.is_empty() {
        let mut out = String::new();
        for error in &reg.errors {
            out.push_str(&format!("error: mcp: {}\n", strip_control(error)));
        }
        // A file that exists and was not approved is the one thing the empty
        // state must not hide: the next start asks again.
        if let Some(file) = reg.project.as_deref().filter(|_| !reg.trusted) {
            out.push_str(&format!(
                "no MCP servers running\n  {}: not approved — the next start asks\n",
                file.display()
            ));
            return out;
        }
        out.push_str(&empty_state());
        return out;
    }
    let mut out = String::from("mcp servers\n");
    for (at, entry) in reg.entries.iter().enumerate() {
        let status = match (&reg.running[at], &reg.failures[at], entry.enabled) {
            (_, _, false) => "disabled".to_string(),
            (Some(server), _, _) => {
                format!("{} tool{}", server.tools.len(), plural(server.tools.len()))
            }
            (None, Some(e), _) => format!("failed: {e}"),
            (None, None, _) => "failed".to_string(),
        };
        out.push_str(&format!(
            "  {:<10} {:<8} {}\n",
            strip_control(&entry.name),
            if entry.project { "project" } else { "global" },
            strip_control(&status),
        ));
    }
    if let Some(file) = &reg.project {
        out.push_str(&format!(
            "  {}: {}\n",
            file.display(),
            if reg.trusted {
                "approved"
            } else {
                "not approved"
            }
        ));
    }
    for error in &reg.errors {
        out.push_str(&format!("  error: {}\n", strip_control(error)));
    }
    if reg.changed_since_start() {
        out.push_str("  config changed since startup — restart axe\n");
    }
    out
}

fn detail(name: &str) -> String {
    let reg = lock();
    let Some(at) = reg.index(name) else {
        return format!("error: mcp: no server named \"{name}\" (see /mcp)\n");
    };
    let entry = &reg.entries[at];
    let mut out = format!(
        "{}  {}\n  {}\n",
        entry.name,
        origin(entry),
        entry.describe()
    );
    match (&reg.running[at], &reg.failures[at], entry.enabled) {
        (_, _, false) => out.push_str("  disabled\n"),
        (Some(server), _, _) => {
            for tool in &server.tools {
                out.push_str(&format!("  {}\n", strip_control(&tool.exposed)));
            }
            out.push_str(&format!(
                "  {} tool{}\n",
                server.tools.len(),
                plural(server.tools.len())
            ));
        }
        (None, Some(e), _) => out.push_str(&format!("  failed: {}\n", strip_control(e))),
        (None, None, _) => out.push_str("  failed\n"),
    }
    out
}

fn empty_state() -> String {
    format!(
        "no MCP servers.\n\n\
         add one with the command that starts it:\n  \
         /mcp add github npx -y @modelcontextprotocol/server-github\n     \
         global: every directory.\n\n  \
         /mcp add --project mytool /opt/bin/mytool serve\n     \
         project: this repository only, written to <repo>/.mcp.json and\n     \
         asked about once before it runs.\n\n  \
         --env NAME   hand it a variable from your environment (repeatable)\n  \
         a new server appears after a restart; /mcp then lists it.\n\n\
         a project server with the same name replaces the global one.\n\
         the global file, if you would rather edit it:\n  \
         {}\n",
        global_path().display()
    )
}

impl Registry {
    fn changed_since_start(&self) -> bool {
        self.stamps.iter().any(|(path, before)| {
            let now = stamp(path);
            match (before, &now) {
                (None, None) => false,
                (Some(before), Some(now)) => {
                    before.len != now.len || before.modified != now.modified
                }
                _ => true,
            }
        })
    }
}

/// `/mcp add`'s flags. `--env` takes a variable **name**: axe reads the value
/// from its own environment, never from the line or the file.
fn add_flags(rest: &str) -> Result<(bool, Vec<String>, Vec<&str>), String> {
    let mut project = false;
    let mut env = Vec::new();
    let mut words = Vec::new();
    let mut iter = rest.split_whitespace();
    while let Some(word) = iter.next() {
        match word {
            "--project" => project = true,
            "--env" => match iter.next() {
                Some(name) if valid_var(name) => env.push(name.to_string()),
                Some(name) => return Err(format!("\"{name}\" is not a variable name")),
                None => return Err("--env needs a variable name".into()),
            },
            word => words.push(word),
        }
    }
    Ok((project, env, words))
}

/// `/mcp add [--project] [--env NAME]... <name> <command> [args...]`. Writes the entry whole
/// and says what to do next: the catalogue is read at startup.
fn add(rest: &str) -> String {
    let usage = "usage: /mcp add [--project] [--env NAME]... <name> <command> [args...]\n";
    let (project, env, words) = match add_flags(rest) {
        Ok(flags) => flags,
        Err(e) => return format!("error: mcp: {e}\n"),
    };
    let Some(name) = words.first().copied() else {
        return usage.into();
    };
    if !valid_name(name) {
        return "error: mcp: names use letters, digits, \"_\" and \"-\" (32 at most)\n".into();
    }
    let Some(command) = words.get(1).copied() else {
        return usage.into();
    };
    let cwd = lock().cwd.clone();
    let mut entry = json!({ "command": command });
    if words.len() > 2 {
        let args: Vec<Value> = words[2..].iter().map(|word| json!(word)).collect();
        entry["args"] = json!(args);
    }
    if !env.is_empty() {
        entry["env"] = json!(env);
    }
    let path = match (project, cwd.clone()) {
        (true, Some(dir)) => project_target(&dir),
        (true, None) => return "error: mcp: no working directory for a project file\n".into(),
        (false, _) => global_path(),
    };
    match write_entry(&path, name, &entry) {
        Ok(()) => {
            // The user just wrote this file through axe: the next start must
            // not ask them to approve their own edit.
            if path != global_path() {
                if let (Some(dir), Ok(bytes)) = (cwd.as_deref(), fs::read(&path)) {
                    remember(dir, &path, &hash_of(&bytes));
                }
            }
            format!(
                "◦ mcp: added \"{name}\" to {}\n  restart axe to load it\n",
                path.display()
            )
        }
        Err(e) => format!("error: mcp: {e}\n"),
    }
}

fn remove(rest: &str) -> String {
    let name = rest.trim();
    if name.is_empty() || name.split_whitespace().count() != 1 {
        return "usage: /mcp remove <name>\n".into();
    }
    let cwd = lock().cwd.clone();
    let mut files = vec![global_path()];
    if let Some(dir) = cwd.as_deref() {
        if let Some(file) = project_file(dir) {
            files.push(file);
        }
    }
    let mut removed = 0;
    for file in files {
        match remove_entry(&file, name) {
            Ok(true) => {
                removed += 1;
                if file != global_path() {
                    if let (Some(dir), Ok(bytes)) = (cwd.as_deref(), fs::read(&file)) {
                        remember(dir, &file, &hash_of(&bytes));
                    }
                }
            }
            Ok(false) => {}
            Err(e) => return format!("error: mcp: {e}\n"),
        }
    }
    match removed {
        0 => format!("error: mcp: no entry named \"{name}\" in either file\n"),
        _ => format!("◦ mcp: removed \"{name}\"; restart axe\n"),
    }
}

fn forget() -> String {
    let cwd = lock().cwd.clone();
    match cwd {
        Some(dir) => format!("◦ {}\n", forget_trust(&dir)),
        None => "error: mcp: no working directory\n".into(),
    }
}

fn read_config(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw).map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn write_config(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let text = format!("{text}\n");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
    }
    match path == global_path() {
        true => state::write_private(path, &text).map_err(|e| format!("{}: {e}", path.display())),
        false => fs::write(path, text).map_err(|e| format!("{}: {e}", path.display())),
    }
}

fn write_entry(path: &Path, name: &str, entry: &Value) -> Result<(), String> {
    let mut config = read_config(path)?;
    if !config.is_object() {
        return Err(format!("{}: expected an object", path.display()));
    }
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }
    let Some(servers) = config["mcpServers"].as_object_mut() else {
        return Err(format!(
            "{}: \"mcpServers\" must be an object",
            path.display()
        ));
    };
    servers.insert(name.to_string(), entry.clone());
    write_config(path, &config)
}

fn remove_entry(path: &Path, name: &str) -> Result<bool, String> {
    let mut config = read_config(path)?;
    let Some(servers) = config["mcpServers"].as_object_mut() else {
        return Ok(false);
    };
    if servers.remove(name).is_none() {
        return Ok(false);
    }
    write_config(path, &config)?;
    Ok(true)
}

/// Char-safe cut, marked at the end: a description or a stderr tail longer
/// than the cap must not split a character.
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(name: &str, command: &str) -> Entry {
        Entry {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
            enabled: true,
            timeout: CALL_TIMEOUT,
            source: PathBuf::from("/tmp/mcp.json"),
            project: false,
            toggle_only: false,
        }
    }

    #[test]
    fn names_are_the_ones_both_sides_can_use() {
        assert!(valid_name("files"));
        assert!(valid_name("github-issues"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a/b"));
        assert!(valid_name(&"a".repeat(32)));
        assert!(!valid_name(&"a".repeat(33)));
    }

    #[test]
    fn tool_names_are_namespaced_and_never_collide() {
        let mut taken = HashSet::new();
        let name = tool_name("files", "read_text_file", &taken);
        assert_eq!(name, "mcp__files__read_text_file");
        taken.insert(name);
        assert_ne!(
            tool_name("files", "read_text_file", &taken),
            "mcp__files__read_text_file"
        );
        // Dots and slashes become the one character a provider allows.
        assert_eq!(tool_name("a.b", "x/y", &HashSet::new()), "mcp__a_b__x_y");
        // Overlong names are cut and tagged, never over the limit.
        let long = tool_name("s", &"t".repeat(80), &HashSet::new());
        assert!(long.len() <= NAME_CAP, "{}", long.len());
        assert!(
            long.ends_with(&format!("_{}", &long[long.len() - 8..])),
            "{long}"
        );
        assert!(long.starts_with("mcp__s__t"));
    }

    #[test]
    fn a_project_entry_replaces_or_toggles_a_global_one() {
        let mut errors = Vec::new();
        let mut replacement = entry("files", "other");
        replacement.project = true;
        let mut off = entry("git", "");
        off.project = true;
        off.toggle_only = true;
        off.enabled = false;
        let merged = merge(
            vec![entry("files", "npx"), entry("git", "mcp-git")],
            vec![replacement, off],
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(merged.len(), 2);
        let files = merged.iter().find(|e| e.name == "files").unwrap();
        assert_eq!(files.command, "other");
        assert!(files.project);
        assert!(!merged.iter().find(|e| e.name == "git").unwrap().enabled);
        // A toggle that names nothing is an error, not a server.
        let mut lonely = entry("nope", "");
        lonely.toggle_only = true;
        let mut errors = Vec::new();
        assert!(merge(Vec::new(), vec![lonely], &mut errors).is_empty());
        assert!(errors[0].contains("no global server"), "{errors:?}");
    }

    #[test]
    fn entries_are_validated_at_the_boundary() {
        let file = Path::new("/tmp/mcp.json");
        let ok = parse_entry(
            "files",
            &json!({ "command": "npx", "args": ["-y", "s"], "env": ["TOKEN"] }),
            file,
            false,
        )
        .unwrap();
        assert_eq!(ok.args, vec!["-y".to_string(), "s".to_string()]);
        assert_eq!(ok.env, vec!["TOKEN".to_string()]);
        assert!(ok.enabled);
        assert_eq!(ok.timeout, CALL_TIMEOUT);

        // Remote is the next feature, and says so.
        let err = parse_entry("x", &json!({ "url": "https://e/mcp" }), file, false).unwrap_err();
        assert!(err.contains("stdio"), "{err}");
        // Values in a file are never read.
        let err = parse_entry(
            "x",
            &json!({ "command": "c", "env": { "A": "B" } }),
            file,
            false,
        )
        .unwrap_err();
        assert!(err.contains("list of variable names"), "{err}");
        // A field axe would ignore must not run half a server.
        let err =
            parse_entry("x", &json!({ "command": "c", "cwd": "/" }), file, false).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
        // Only the project may narrow; it can never define.
        assert!(parse_entry("x", &json!({ "enabled": false }), file, false).is_err());
        assert!(parse_entry("x", &json!({ "enabled": false }), file, true).is_ok());
        assert!(parse_entry("x", &json!({}), file, true).is_err());
        assert!(parse_entry("x", &json!({ "command": "c", "timeout": 0 }), file, false).is_err());
        assert!(parse_entry(
            "x",
            &json!({ "command": "c", "enabled": "yes" }),
            file,
            false
        )
        .is_err());
        assert!(parse_entry("x", &json!({ "command": "c", "args": "one" }), file, false).is_err());
        assert!(parse_entry(
            "x",
            &json!({ "command": "c", "env": ["1BAD"] }),
            file,
            false
        )
        .is_err());
    }

    #[test]
    fn a_result_is_text_or_an_error() {
        let text = render(&json!({ "content": [
            { "type": "text", "text": "one" },
            { "type": "image", "data": "…" },
            { "type": "text", "text": "two" },
        ]}));
        assert_eq!(text, "one\n[image omitted]\ntwo");
        assert_eq!(render(&json!({ "content": [], "isError": false })), "done");
        assert_eq!(
            render(&json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true })),
            "error: boom"
        );
        // Control bytes from a server never reach the terminal.
        assert_eq!(
            render(&json!({ "content": [{ "type": "text", "text": "a\u{1b}[31m" }] })),
            "a[31m"
        );
    }

    #[test]
    fn a_long_result_is_cut_and_kept_whole() {
        let text = "x".repeat(RESULT_CAP + 100);
        let rendered = render(&json!({ "content": [{ "type": "text", "text": text }] }));
        assert!(rendered.contains("full output"));
        assert!(
            rendered.chars().count() <= RESULT_CAP + 120,
            "{}",
            rendered.len()
        );
        let path = rendered
            .rsplit_once("[full output: ")
            .and_then(|(_, rest)| rest.strip_suffix("]"))
            .unwrap();
        let saved = fs::read_to_string(path).unwrap();
        assert_eq!(saved.chars().count(), RESULT_CAP + 100);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn trust_is_by_path_and_content() {
        // The record lives under the state root, so the test needs a scratch home.
        state::testenv::home("mcp-trust", |home| {
            let dir = home.join("repo");
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join(".mcp.json");
            fs::write(&file, "{}").unwrap();
            let hash = hash_of(b"{}");
            assert!(!approved(&dir, &file, &hash));
            remember(&dir, &file, &hash);
            assert!(approved(&dir, &file, &hash));
            assert!(!approved(&dir, &file, &hash_of(b"{\"mcpServers\":{}}")));
            let mode = fs::metadata(trust_file(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the record is the user's");
            assert!(forget_trust(&dir).contains("forgotten"));
            assert!(!approved(&dir, &file, &hash));
        });
    }

    #[test]
    fn the_project_file_stops_at_the_repository_root() {
        let dir = state::testenv::dir("mcp-project");
        let repo = dir.join("repo");
        let nested = repo.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        assert!(project_file(&nested).is_none());
        fs::write(repo.join(".mcp.json"), "{}").unwrap();
        assert_eq!(project_file(&nested), Some(repo.join(".mcp.json")));
        // A parent's file is not this checkout's.
        fs::write(dir.join(".mcp.json"), "{}").unwrap();
        assert_eq!(project_file(&nested), Some(repo.join(".mcp.json")));
        assert_eq!(project_target(&nested), repo.join(".mcp.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_flags_carry_a_project_and_variable_names() {
        let (project, env, words) = add_flags("--env TOKEN github npx -y s").unwrap();
        assert!(!project);
        assert_eq!(env, vec!["TOKEN".to_string()]);
        assert_eq!(words, vec!["github", "npx", "-y", "s"]);
        let (project, _, words) = add_flags("--project mytool /bin/sh serve").unwrap();
        assert!(project);
        assert_eq!(words, vec!["mytool", "/bin/sh", "serve"]);
        assert!(add_flags("--env").unwrap_err().contains("needs a variable"));
        assert!(add_flags("--env 1BAD x c")
            .unwrap_err()
            .contains("not a variable"));
    }

    #[test]
    fn an_add_writes_the_entry_and_a_remove_takes_it_away() {
        let dir = state::testenv::dir("mcp-add");
        let file = dir.join("mcp.json");
        write_entry(
            &file,
            "files",
            &json!({ "command": "npx", "args": ["-y", "s"] }),
        )
        .unwrap();
        let raw = fs::read_to_string(&file).unwrap();
        assert!(raw.contains("\"mcpServers\""), "{raw}");
        assert!(raw.contains("\"command\": \"npx\""), "{raw}");
        let (entries, errors) = parse_servers(&file, false);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), 1);
        assert!(remove_entry(&file, "files").unwrap());
        assert!(!remove_entry(&file, "files").unwrap());
        assert!(parse_servers(&file, false).0.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_env_name_resolves_or_is_named() {
        assert!(valid_var("GITHUB_TOKEN"));
        assert!(valid_var("_x1"));
        assert!(!valid_var("1X"));
        assert!(!valid_var("A-B"));
        std::env::set_var("AXE_TEST_MCP_TOKEN", "secret");
        let mut e = entry("x", "/bin/sh");
        e.env = vec!["AXE_TEST_MCP_TOKEN".into()];
        std::env::remove_var("AXE_TEST_MCP_TOKEN");
        let err = Server::open(&e, false).err().unwrap();
        assert!(err.contains("AXE_TEST_MCP_TOKEN is not set"), "{err}");
    }

    #[test]
    fn a_stdio_server_is_asked_for_its_tools_and_answers_a_call() {
        // A whole server in one line of shell: initialize, list, call.
        let script = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t","version":"1"}}}'
read line
read line
echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"say it back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"said it\"}]}}"
done
"#;
        let dir = state::testenv::dir("mcp-server");
        let file = dir.join("server.sh");
        fs::write(&file, script).unwrap();
        let mut e = entry("t", "/bin/sh");
        e.args = vec![file.display().to_string()];
        let server = Server::open(&e, false).expect("a server that answers");
        assert_eq!(server.listed.len(), 1);
        assert_eq!(server.listed[0].original, "echo");
        let mut running = vec![Some(server)];
        name_tools(&mut running);
        let server = running[0].as_mut().unwrap();
        assert_eq!(server.tools[0].exposed, "mcp__t__echo");
        let result = server.call("echo", &json!({ "text": "hi" }), &Cancel::new());
        assert_eq!(result, "said it");
        // A dead server says so rather than hanging.
        server.stop();
        assert!(server
            .call("echo", &json!({}), &Cancel::new())
            .contains("error: mcp t"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The two events a server sends before it matters, then one reply per
    /// call: enough of a server to prove the kernel rule.
    fn writer(path: &Path) -> String {
        format!(
            r#"
read line
echo '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"t","version":"1"}}}}}}'
read line
read line
echo '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"write","inputSchema":{{"type":"object"}}}}]}}}}'
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  out=$(printf x > {} 2>&1; printf '%s' "$?")
  echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"$out\"}}]}}}}"
done
"#,
            path.display()
        )
    }

    fn run_writer(explore: bool, target: &Path) -> String {
        let dir = state::testenv::dir(&format!("mcp-write-{explore}"));
        let file = dir.join("server.sh");
        fs::write(&file, writer(target)).unwrap();
        let mut e = entry("t", "/bin/sh");
        e.args = vec![file.display().to_string()];
        let server = Server::open(&e, explore).expect("a server that answers");
        let mut running = vec![Some(server)];
        name_tools(&mut running);
        let server = running[0].as_mut().unwrap();
        let result = server.call("write", &json!({}), &Cancel::new());
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn explore_mode_confines_a_servers_writes() {
        let dir = state::testenv::dir("mcp-confined");
        let target = dir.join("out");
        let refused = run_writer(true, &target);
        assert!(!target.exists(), "the kernel refused: {refused}");
        assert_ne!(refused, "0", "{refused}");
        let allowed = run_writer(false, &target);
        assert_eq!(allowed, "0", "{allowed}");
        assert!(target.exists(), "normal mode writes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupt_stops_waiting_and_leaves_the_server_running() {
        let dir = state::testenv::dir("mcp-cancel");
        let script = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t","version":"1"}}}'
read line
read line
echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"slow","inputSchema":{"type":"object"}}]}}'
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  sleep 0.3
  echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"late\"}]}}"
done
"#;
        let file = dir.join("server.sh");
        fs::write(&file, script).unwrap();
        let mut e = entry("t", "/bin/sh");
        e.args = vec![file.display().to_string()];
        let server = Server::open(&e, false).unwrap();
        let mut running = vec![Some(server)];
        name_tools(&mut running);
        let server = running[0].as_mut().unwrap();
        let cancel = Arc::new(Cancel::new());
        let trigger = cancel.clone();
        let flag = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            trigger.cancel();
        });
        let result = server.call("slow", &json!({}), &cancel);
        flag.join().unwrap();
        assert!(result.contains("interrupted"), "{result}");
        // The process is not killed: the next call is answered by the same one.
        assert_eq!(server.call("slow", &json!({}), &Cancel::new()), "late");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_start_leaves_the_catalogue_alone() {
        let dir = state::testenv::dir("mcp-empty");
        state::testenv::home("mcp-empty-home", |_home| {
            let notices = start(Some(&dir), false, false);
            assert!(notices.iter().all(|n| !n.contains("tool")), "{notices:?}");
            assert!(definitions().is_empty());
            let listing = dispatch("");
            assert!(listing.contains("no MCP servers"), "{listing}");
            assert!(listing.contains("mcp.json"), "{listing}");
            shutdown();
        });
        let _ = fs::remove_dir_all(&dir);
    }
}
