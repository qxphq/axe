//! MCP servers: the two config files, the connections axe makes from them, and
//! the tools they add. A server is a program (`command`) or an endpoint
//! (`url`, streamable HTTP); both halves speak the same JSON-RPC and produce
//! the same `mcp__<server>__<tool>` names in one catalogue fixed at startup.
//!
//! The global file is `~/.config/axe/mcp.json`; a project's is `.mcp.json` at
//! or above the working directory, never past its repository root. A project
//! file is code a repository shipped, so it runs only after one approval per
//! file, remembered by content hash in the project's state directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::{self, Cancel};
use crate::state;
use crate::text::{strip_control, truncate};

/// The revision axe speaks.
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
    /// A program axe starts and speaks to over its stdin/stdout.
    command: Option<String>,
    /// An endpoint axe posts to: streamable HTTP.
    url: Option<String>,
    args: Vec<String>,
    /// Variable name -> raw value; `${NAME}` is expanded from axe's
    /// environment when the server starts, so the prompt can name it before.
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    /// Every `${NAME}`/`$NAME` the entry reads, for the approval prompt.
    vars: Vec<String>,
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
    fn remote(&self) -> bool {
        self.url.is_some()
    }

    fn describe(&self) -> String {
        if self.toggle_only {
            return if self.enabled {
                "turns the global server on".into()
            } else {
                "turns the global server off".into()
            };
        }
        if let Some(url) = &self.url {
            let mut line = url.clone();
            for (name, _) in &self.headers {
                line.push_str(&format!("  header: {name}"));
            }
            return line;
        }
        let mut line = self.command.clone().unwrap_or_default();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        for (name, _) in &self.env {
            line.push_str(&format!("  env: {name}"));
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

/// The variable names a value reads: `$NAME` or `${NAME}`, `$$` an escape.
fn vars_in(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut names = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] != '$' {
            at += 1;
            continue;
        }
        if chars.get(at + 1) == Some(&'$') {
            at += 2;
            continue;
        }
        if chars.get(at + 1) == Some(&'{') {
            if let Some(end) = chars[at + 2..].iter().position(|c| *c == '}') {
                let name: String = chars[at + 2..at + 2 + end].iter().collect();
                if valid_var(&name) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                    at += end + 3;
                    continue;
                }
            }
            at += 1;
            continue;
        }
        let start = at + 1;
        let mut end = start;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        let name: String = chars[start..end].iter().collect();
        if end > start && valid_var(&name) {
            if !names.contains(&name) {
                names.push(name);
            }
            at = end;
        } else {
            at += 1;
        }
    }
    names
}

/// Resolve `$NAME`/`${NAME}` from axe's environment. A missing variable is an
/// error naming it: the server must not start with half its configuration.
fn expand(raw: &str) -> Result<String, String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    while at < chars.len() {
        match chars[at] {
            '$' if chars.get(at + 1) == Some(&'$') => {
                out.push('$');
                at += 2;
            }
            '$' if chars.get(at + 1) == Some(&'{') => {
                let end = chars[at + 2..].iter().position(|c| *c == '}');
                match end {
                    Some(end) => {
                        let name: String = chars[at + 2..at + 2 + end].iter().collect();
                        if !valid_var(&name) {
                            out.push('$');
                            at += 1;
                            continue;
                        }
                        out.push_str(&value(&name)?);
                        at += end + 3;
                    }
                    None => {
                        out.push('$');
                        at += 1;
                    }
                }
            }
            '$' => {
                let start = at + 1;
                let mut end = start;
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                if end > start && valid_var(&name) {
                    out.push_str(&value(&name)?);
                    at = end;
                } else {
                    out.push('$');
                    at += 1;
                }
            }
            c => {
                out.push(c);
                at += 1;
            }
        }
    }
    Ok(out)
}

fn value(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// A stdio entry's program and argv, expanded from axe's environment when the
/// server starts. The approval question lists these variables, so they are the
/// ones it must read: a reference in an argument is resolved here, not passed
/// through as text.
fn stdio_words(entry: &Entry) -> Result<(String, Vec<String>), String> {
    let command = entry.command.as_deref().unwrap_or_default();
    let program = expand(command).map_err(|e| format!("command: {e}"))?;
    let mut args = Vec::with_capacity(entry.args.len());
    for (at, arg) in entry.args.iter().enumerate() {
        args.push(expand(arg).map_err(|e| format!("args[{at}]: {e}"))?);
    }
    Ok((program, args))
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
            "command" | "args" | "env" | "url" | "headers" | "enabled" | "timeout" | "type"
        ) {
            return Err(format!(
                "server \"{name}\": unknown key \"{key}\" (axe takes command, args, env, url, \
                 headers, enabled, timeout)"
            ));
        }
    }
    if let Some(kind) = object.get("type").and_then(Value::as_str) {
        if !matches!(kind, "stdio" | "http" | "streamable-http") {
            return Err(format!(
                "server \"{name}\": type \"{kind}\" is not supported; axe runs stdio and \
                 streamable-http servers"
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
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
        .map(str::to_string);
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string);
    if command.is_some() && url.is_some() {
        return Err(format!(
            "server \"{name}\": an entry is a command or a url, not both"
        ));
    }
    if let Some(url) = &url {
        parse_url_value(name, url)?;
        if object.contains_key("args") || object.contains_key("env") {
            return Err(format!(
                "server \"{name}\": \"args\" and \"env\" belong to a command, not a url"
            ));
        }
    }
    let env = parse_env(name, object.get("env"))?;
    let headers = parse_headers(name, object.get("headers"))?;
    if command.is_some() && !headers.is_empty() {
        return Err(format!(
            "server \"{name}\": \"headers\" belongs to a url, not a command"
        ));
    }
    if command.is_none() && url.is_none() {
        // A project entry with no command can only switch a global server: a
        // repository may narrow what runs, never define it.
        if !project {
            return Err(format!("server \"{name}\": missing \"command\" or \"url\""));
        }
        if object.get("enabled").is_none() {
            return Err(format!(
                "server \"{name}\": needs \"command\"/\"url\", or \"enabled\" to switch a \
                 global server"
            ));
        }
        if !env.is_empty() || !headers.is_empty() || object.contains_key("args") {
            return Err(format!(
                "server \"{name}\": an entry without \"command\" or \"url\" may only set \
                 \"enabled\""
            ));
        }
        return Ok(Entry {
            name: name.to_string(),
            command: None,
            url: None,
            args: Vec::new(),
            env,
            headers,
            vars: Vec::new(),
            enabled,
            timeout,
            source: path.to_path_buf(),
            project,
            toggle_only: true,
        });
    }
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
    let command = command.map(|command| expand_home(&command));
    let mut vars = Vec::new();
    let mut note = |raw: &str| {
        for name in vars_in(raw) {
            if !vars.contains(&name) {
                vars.push(name);
            }
        }
    };
    if let Some(command) = &command {
        note(command);
    }
    for arg in &args {
        note(arg);
    }
    for (_, raw) in &env {
        note(raw);
    }
    for (_, raw) in &headers {
        note(raw);
    }
    Ok(Entry {
        name: name.to_string(),
        command,
        url,
        args,
        env,
        headers,
        vars,
        enabled,
        timeout,
        source: path.to_path_buf(),
        project,
        toggle_only: false,
    })
}

fn parse_url_value(name: &str, url: &str) -> Result<(), String> {
    http::split_url(url).map_err(|e| format!("server \"{name}\": \"url\": {e}"))?;
    Ok(())
}

/// `env` maps a name to a value or a reference. An array of names is the
/// shorthand for "pass it through unchanged", so the two spellings agree.
fn parse_env(name: &str, value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut pairs = Vec::new();
            for item in items {
                let raw = item.as_str().ok_or_else(|| {
                    format!("server \"{name}\": \"env\" must map names to values")
                })?;
                if !valid_var(raw) {
                    return Err(format!(
                        "server \"{name}\": env \"{raw}\" is not a variable name"
                    ));
                }
                pairs.push((raw.to_string(), format!("${{{raw}}}")));
            }
            Ok(pairs)
        }
        Some(Value::Object(map)) => {
            let mut pairs = Vec::new();
            for (key, value) in map {
                if !valid_var(key) {
                    return Err(format!(
                        "server \"{name}\": env \"{key}\" is not a variable name"
                    ));
                }
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("server \"{name}\": env \"{key}\" must be a string"))?;
                pairs.push((key.clone(), raw.to_string()));
            }
            Ok(pairs)
        }
        Some(_) => Err(format!(
            "server \"{name}\": \"env\" maps names to values, or lists names"
        )),
    }
}

fn parse_headers(name: &str, value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Object(map)) => {
            let mut pairs = Vec::new();
            for (key, value) in map {
                if !http::valid_header_name(key) {
                    return Err(format!("server \"{name}\": \"{key}\" is not a header name"));
                }
                let raw = value.as_str().ok_or_else(|| {
                    format!("server \"{name}\": header \"{key}\" must be a string")
                })?;
                pairs.push((key.clone(), raw.to_string()));
            }
            Ok(pairs)
        }
        Some(_) => Err(format!(
            "server \"{name}\": \"headers\" must map names to strings"
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

/// The digest of a file's bytes, from the host's `sha256sum` — or `shasum -a
/// 256`, which is the name macOS has. The approval binds the content, so it
/// must not ride on a hash that can be inverted or collided; `None` when the
/// host has neither, and the file goes unapproved rather than trusted on a
/// weaker sum.
fn hash_of(bytes: &[u8]) -> Option<String> {
    digest("sha256sum", &[], bytes).or_else(|| digest("shasum", &["-a", "256"], bytes))
}

fn digest(program: &str, args: &[&str], bytes: &[u8]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    stdin.write_all(bytes).ok()?;
    drop(stdin);
    let out = child.wait_with_output().ok()?;
    if !out.status.success() || out.stdout.len() > 128 {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let hex = text.split_whitespace().next()?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_string())
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
    if let Err(e) = write_atomic(&trust_file(cwd), &text, true) {
        eprintln!("axe: mcp trust: {e}");
    }
}

/// Carry a project file's approval across a write axe itself made — but only
/// when the file was already approved. An add or a remove must never bless
/// entries the approval question has not listed: the file is code, and the
/// question is the only place it is shown.
fn carry_approval(cwd: &Path, file: &Path, prior: Option<&str>) {
    match prior {
        Some(hash) if approved(cwd, file, hash) => {}
        _ => return,
    }
    if let Ok(bytes) = fs::read(file) {
        if let Some(hash) = hash_of(&bytes) {
            remember(cwd, file, &hash);
        }
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
        if !entry.vars.is_empty() {
            text.push_str(&format!("     reads: ${}\n", entry.vars.join(", $")));
        }
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

// ---- the connections -------------------------------------------------------

struct StdioConn {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    /// Replies by id, filled by the reader thread.
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    dead: Arc<AtomicBool>,
    /// The reader stopped on a line over the cap, not on an exit: the process
    /// is alive and its output is simply not read any more.
    over_cap: Arc<AtomicBool>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// The process group is gone. A reaped pid may be reused, so this is what
    /// keeps `stop` from signalling a group that is not ours any more.
    stopped: bool,
}

struct HttpConn {
    ep: http::Endpoint,
    session: Option<String>,
    /// Configured headers, values already expanded.
    headers: Vec<(String, String)>,
}

enum Conn {
    Stdio(StdioConn),
    Http(HttpConn),
}

impl Conn {
    fn exchange(
        &mut self,
        message: &Value,
        id: u64,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Value, String> {
        match self {
            Conn::Stdio(conn) => conn.exchange(message, id, timeout, cancel),
            Conn::Http(conn) => conn.exchange(message, id, timeout, cancel),
        }
    }

    fn notify(&mut self, message: &Value) -> Result<(), String> {
        match self {
            Conn::Stdio(conn) => conn.notify(message),
            Conn::Http(conn) => conn.notify(message),
        }
    }

    fn stop(&mut self) {
        match self {
            Conn::Stdio(conn) => conn.stop(),
            Conn::Http(conn) => conn.stop(),
        }
    }
}

impl StdioConn {
    fn open(entry: &Entry, explore: bool) -> Result<StdioConn, String> {
        let (command, args) = stdio_words(entry)?;
        let program = match command.contains('/') {
            true => PathBuf::from(&command),
            false => crate::sandbox::resolve_program(&command)
                .ok_or_else(|| format!("\"{command}\" is not on PATH"))?,
        };
        let mut env = Vec::new();
        for (name, raw) in &entry.env {
            env.push((
                name.clone(),
                expand(raw).map_err(|e| format!("{name}: {e}"))?,
            ));
        }
        let mut cmd = crate::sandbox::server(&program, &args, &env, explore)?;
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
        let over_cap = Arc::new(AtomicBool::new(false));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(stdout) = stdout {
            let pending = pending.clone();
            let dead = dead.clone();
            let capped = over_cap.clone();
            thread::spawn(move || read_replies(stdout, pending, dead, capped));
        }
        if let Some(pipe) = stderr_pipe {
            let tail = stderr.clone();
            thread::spawn(move || read_stderr(pipe, tail));
        }
        Ok(StdioConn {
            child,
            pid,
            stdin,
            pending,
            dead,
            over_cap,
            stderr,
            stopped: false,
        })
    }

    fn notify(&mut self, message: &Value) -> Result<(), String> {
        self.write(message)
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
    fn exchange(
        &mut self,
        message: &Value,
        id: u64,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Value, String> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(self.gone());
        }
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }
        if let Err(e) = self.write(message) {
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
                    if cancel.cancelled() {
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
            if cancelled || cancel.cancelled() {
                let _ = self.notify(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": { "requestId": id, "reason": "interrupted by user" },
                }));
                return Err("interrupted by user (ESC); the call was not run".into());
            }
            if self.dead.load(Ordering::SeqCst) {
                return Err(self.exited());
            }
            let _ = self.notify(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": id, "reason": "timeout" },
            }));
            return Err(format!("timed out after {}s", timeout.as_secs()));
        };
        if reply.is_null() {
            return Err(self.gone());
        }
        Ok(reply)
    }

    /// What to say when the connection is gone: an oversized line is not an
    /// exit, and saying so is how the model tells a protocol limit from a
    /// crash.
    fn gone(&self) -> String {
        match self.over_cap.load(Ordering::SeqCst) {
            true => cap_reason(),
            false => self.exited(),
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
            out.push_str(&cut(&strip_control(&tail), 800));
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

impl Drop for StdioConn {
    fn drop(&mut self) {
        self.stop();
    }
}

impl HttpConn {
    fn open(entry: &Entry) -> Result<HttpConn, String> {
        let url = entry.url.clone().unwrap_or_default();
        let (ep, _) = http::split_url(&url)?;
        let mut headers = Vec::new();
        for (name, raw) in &entry.headers {
            headers.push((
                name.clone(),
                expand(raw).map_err(|e| format!("header {name}: {e}"))?,
            ));
        }
        Ok(HttpConn {
            ep,
            session: None,
            headers,
        })
    }

    fn path(&self) -> &str {
        if self.ep.prefix.is_empty() {
            "/"
        } else {
            ""
        }
    }

    fn base_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ),
            ("mcp-protocol-version".to_string(), PROTOCOL.to_string()),
        ];
        if let Some(session) = &self.session {
            headers.push(("mcp-session-id".to_string(), session.clone()));
        }
        headers.extend(self.headers.iter().cloned());
        headers
    }

    fn post(
        &self,
        body: &str,
        headers: &[(String, String)],
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<http::Response, String> {
        http::request_with_timeout(
            &self.ep,
            "POST",
            self.path(),
            None,
            headers,
            Some(body),
            None,
            Some(cancel),
            timeout,
        )
        .map_err(|e| match cancel.cancelled() {
            true => "interrupted by user (ESC); the call was not run".to_string(),
            false => e,
        })
    }

    fn notify(&mut self, message: &Value) -> Result<(), String> {
        let headers = self.base_headers();
        let cancel = Cancel::new();
        let response = self.post(&message.to_string(), &headers, INIT_TIMEOUT, &cancel)?;
        if let Some(session) = header(&response, "mcp-session-id") {
            self.session = Some(session);
        }
        Ok(())
    }

    fn exchange(
        &mut self,
        message: &Value,
        id: u64,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Result<Value, String> {
        let body = message.to_string();
        let headers = self.base_headers();
        let mut response = self.post(&body, &headers, timeout, cancel)?;
        if let Some(session) = header(&response, "mcp-session-id") {
            self.session = Some(session);
        }
        if !(200..300).contains(&response.status) {
            let text = response.read_all().unwrap_or_default();
            let message = short(&text);
            return Err(match message.is_empty() {
                true => format!("HTTP {}", response.status),
                false => format!("HTTP {}: {message}", response.status),
            });
        }
        if response.status == 202 {
            return Ok(Value::Null);
        }
        reply(&mut response, id, timeout, cancel)
    }

    /// Best effort: tell the server the session is over.
    fn stop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let mut headers = self.base_headers();
        headers.push(("mcp-session-id".to_string(), session));
        let cancel = Cancel::new();
        let _ = http::request_with_timeout(
            &self.ep,
            "DELETE",
            self.path(),
            None,
            &headers,
            None,
            None,
            Some(&cancel),
            Duration::from_secs(5),
        );
    }
}

fn header(response: &http::Response, name: &str) -> Option<String> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// One HTTP reply: a JSON body, or an SSE stream carrying the message. The
/// server may interleave notifications; only the id axe asked for ends the
/// wait.
fn reply(
    response: &mut http::Response,
    id: u64,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<Value, String> {
    let sse = response
        .content_type
        .as_deref()
        .is_some_and(|kind| kind.contains("text/event-stream"));
    if !sse {
        let body = response.read_all().map_err(|e| format!("read: {e}"))?;
        let value: Value =
            serde_json::from_str(&body).map_err(|e| format!("reply: {e}: {}", short(&body)))?;
        if value["id"].as_u64() == Some(id) {
            return Ok(value);
        }
        return Err("the reply carried another id".into());
    }
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.cancelled() {
            return Err("interrupted by user (ESC); the call was not run".into());
        }
        if Instant::now() > deadline {
            return Err(format!("timed out after {}s", timeout.as_secs()));
        }
        let line = match response.read_line() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => match cancel.cancelled() {
                true => return Err("interrupted by user (ESC); the call was not run".into()),
                false => return Err(format!("read: {e}")),
            },
        };
        let Some(data) = line.trim_end().strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if value["id"].as_u64() == Some(id) {
            return Ok(value);
        }
    }
    Err("the stream ended without a reply".into())
}

/// Replies, by id. Notifications are dropped: axe advertises no capability
/// that would make one its business, and a list change is not followed — the
/// catalogue is fixed for the session.
fn read_replies(
    stdout: impl Read,
    pending: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    dead: Arc<AtomicBool>,
    over_cap: Arc<AtomicBool>,
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
            // A line over the cap is a protocol limit, not an exit: the process
            // may live on, and the answer says which one happened.
            Ok(_) if line.len() > LINE_CAP => {
                over_cap.store(true, Ordering::SeqCst);
                break;
            }
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

// ---- one server and its tools ----------------------------------------------

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
    timeout: Duration,
    next: u64,
    listed: Vec<Listed>,
    tools: Vec<Tool>,
    conn: Conn,
}

impl Server {
    fn open(entry: &Entry, explore: bool) -> Result<Server, String> {
        let conn = match &entry.command {
            Some(_) => Conn::Stdio(StdioConn::open(entry, explore)?),
            None => Conn::Http(HttpConn::open(entry)?),
        };
        let mut server = Server {
            name: entry.name.clone(),
            timeout: Duration::from_secs(entry.timeout),
            next: 1,
            listed: Vec::new(),
            tools: Vec::new(),
            conn,
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
        let message =
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} });
        let _ = self.conn.notify(&message);
        Ok(())
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
                    description: cut(&description, DESCRIPTION_CAP),
                    schema,
                });
            }
            cursor = result["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                return Ok(());
            }
        }
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: Option<&Cancel>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let fresh = Cancel::new();
        let cancel = cancel.unwrap_or(&fresh);
        let reply = self.conn.exchange(&message, id, timeout, cancel)?;
        if reply.is_null() {
            return Ok(Value::Null);
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

    fn stop(&mut self) {
        self.conn.stop();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
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
    /// The project's entries are in force.
    trusted: bool,
    /// A trust record matched the file as it stands.
    recorded: bool,
    /// The file defines something, so the question is owed.
    needs_approval: bool,
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
    let mut recorded = false;
    let mut needs_approval = false;
    if let Some(file) = &project {
        let bytes = fs::read(file).unwrap_or_default();
        let hash = hash_of(&bytes);
        recorded = match (&hash, cwd) {
            (Some(hash), Some(dir)) => approved(dir, file, hash),
            _ => false,
        };
        trusted = recorded;
        // A file that only switches global servers runs nothing new: it needs
        // no approval, and its entries are applied either way.
        needs_approval = project_entries.iter().any(|entry| !entry.toggle_only);
        if !trusted && needs_approval {
            match &hash {
                None => errors.push(format!(
                    "{}: no sha256sum or shasum, so it cannot be approved",
                    file.display()
                )),
                Some(hash) => {
                    if interactive && ask(file, &project_entries) {
                        if let Some(dir) = cwd {
                            remember(dir, file, hash);
                        }
                        trusted = true;
                        recorded = true;
                    }
                }
            }
        } else if !needs_approval {
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
                    "mcp {}: {} {}{}",
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
        recorded,
        needs_approval,
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
                    "mcp {}: {} {}{}",
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

/// At exit: no server outlives the session that started it. The registry is
/// taken through `lock`, which recovers a poison a panicking call may have
/// left behind — a failed cleanup must not be the way a server survives.
pub fn shutdown() {
    let mut reg = lock();
    for server in reg.running.iter_mut().flatten() {
        server.stop();
    }
}

// ---- `/mcp` -----------------------------------------------------------------

/// `/mcp`, `/mcp <name>`, and the edits. One entry point so the REPL's dispatch
/// stays a line.
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

/// What the listing says about the project file: the record that matched, and
/// whether the file defines anything the question is owed for.
fn approval_state(recorded: bool, needs_approval: bool) -> &'static str {
    match (recorded, needs_approval) {
        (true, _) => "approved",
        (false, true) => "not approved",
        (false, false) => "nothing to approve",
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
        if let Some(file) = reg
            .project
            .as_deref()
            .filter(|_| reg.needs_approval && !reg.recorded)
        {
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
                format!("{} {}", server.tools.len(), plural(server.tools.len()))
            }
            (None, Some(e), _) => format!("failed: {e}"),
            (None, None, _) => "failed".to_string(),
        };
        let kind = if entry.remote() { "http" } else { "stdio" };
        out.push_str(&format!(
            "  {:<10} {:<8} {:<6} {}\n",
            strip_control(&entry.name),
            if entry.project { "project" } else { "global" },
            kind,
            strip_control(&status),
        ));
    }
    if let Some(file) = &reg.project {
        // The record is what this line reports, not whether entries happen to
        // be in force: a toggle-only file was never asked about, and a file
        // whose approval was just forgotten is not approved any more.
        let state = approval_state(reg.recorded, reg.needs_approval);
        out.push_str(&format!("  {}: {state}\n", file.display()));
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
                "  {} {}\n",
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
         /mcp add --url sentry https://mcp.sentry.dev/mcp\n     \
         a url is streamable HTTP; --header carries its bearer token.\n\n  \
         --env NAME      hand it a variable from your environment (repeatable);\n     \
         NAME=VALUE writes the value as given, so a secret belongs in the first\n  \
         --header 'K: V' one header, for a url (repeatable)\n  \
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

/// Split a line the way a shell would for the two quote kinds: an argument
/// with a space in it is written in quotes. No expansion, no escapes.
fn words(rest: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in rest.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                c => {
                    current.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("unclosed quote".into());
    }
    if started {
        out.push(current);
    }
    Ok(out)
}

/// `/mcp add`'s flags. `--env` takes a variable **name** (meaning
/// `NAME=${NAME}`) or `NAME=VALUE`. The name form is the one that keeps a
/// secret out of the file; a value is written as given, and only `$NAME`
/// inside it is expanded when the server starts.
#[allow(clippy::type_complexity)]
fn add_flags(
    rest: &str,
) -> Result<
    (
        bool,
        Vec<(String, String)>,
        Vec<(String, String)>,
        Option<String>,
        Option<u64>,
        Vec<String>,
    ),
    String,
> {
    let mut project = false;
    let mut env = Vec::new();
    let mut headers = Vec::new();
    let mut url = None;
    let mut timeout = None;
    let mut words_out = Vec::new();
    let words = words(rest)?;
    let mut at = 0;
    while at < words.len() {
        let word = words[at].as_str();
        match word {
            "--project" => project = true,
            "--url" => match words.get(at + 1) {
                Some(value) => {
                    url = Some(value.clone());
                    at += 1;
                }
                None => return Err("--url needs a URL".into()),
            },
            "--timeout" => {
                match words.get(at + 1) {
                    Some(value) => {
                        timeout =
                            Some(value.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(
                                || format!("\"{value}\" is not a number of seconds"),
                            )?);
                        at += 1;
                    }
                    None => return Err("--timeout needs a number of seconds".into()),
                }
            }
            "--env" => match words.get(at + 1) {
                Some(name) if valid_var(name) => {
                    env.push((name.clone(), format!("${{{name}}}")));
                    at += 1;
                }
                Some(pair) if pair.contains('=') => {
                    let (name, value) = pair.split_once('=').unwrap_or_default();
                    if !valid_var(name) {
                        return Err(format!("\"{name}\" is not a variable name"));
                    }
                    if value.is_empty() {
                        return Err(format!("--env {name}= needs a value"));
                    }
                    env.push((name.to_string(), value.to_string()));
                    at += 1;
                }
                Some(other) => return Err(format!("\"{other}\" is not a variable name")),
                None => return Err("--env needs a variable name".into()),
            },
            "--header" => match words.get(at + 1) {
                Some(pair) => {
                    let (name, value) = pair
                        .split_once(':')
                        .ok_or_else(|| "a header is 'Name: value'".to_string())?;
                    if !http::valid_header_name(name.trim()) {
                        return Err(format!("\"{}\" is not a header name", name.trim()));
                    }
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                    at += 1;
                }
                None => return Err("--header needs 'Name: value'".into()),
            },
            word => words_out.push(word.to_string()),
        }
        at += 1;
    }
    Ok((project, env, headers, url, timeout, words_out))
}

fn add(rest: &str) -> String {
    let usage = "usage: /mcp add [--project] [--url URL] [--env NAME[=VALUE]]... \
                 [--header 'K: V']... [--timeout S] <name> [command [args...]]\n";
    let (project, env, headers, url, timeout, words) = match add_flags(rest) {
        Ok(flags) => flags,
        Err(e) => return format!("error: mcp: {e}\n"),
    };
    let Some(name) = words.first().map(String::as_str) else {
        return usage.into();
    };
    if !valid_name(name) {
        return "error: mcp: names use letters, digits, \"_\" and \"-\" (32 at most)\n".into();
    }
    let cwd = lock().cwd.clone();
    let mut entry = json!({});
    if let Some(url) = &url {
        entry["url"] = json!(url);
        if !headers.is_empty() {
            entry["headers"] = json!(headers.iter().cloned().collect::<BTreeMap<_, _>>());
        }
        if !env.is_empty() {
            return "error: mcp: --env belongs to a command, not a url\n".into();
        }
        if words.len() > 1 {
            return "error: mcp: a url entry names no command\n".into();
        }
    } else {
        let Some(command) = words.get(1).map(String::as_str) else {
            return usage.into();
        };
        entry["command"] = json!(command);
        if words.len() > 2 {
            let args: Vec<Value> = words[2..].iter().map(|word| json!(word)).collect();
            entry["args"] = json!(args);
        }
        if !env.is_empty() {
            entry["env"] = json!(env.iter().cloned().collect::<BTreeMap<_, _>>());
        }
        if !headers.is_empty() {
            return "error: mcp: --header belongs to a url, not a command\n".into();
        }
    }
    if let Some(timeout) = timeout {
        entry["timeout"] = json!(timeout);
    }
    let path = match (project, cwd.clone()) {
        (true, Some(dir)) => project_target(&dir),
        (true, None) => return "error: mcp: no working directory for a project file\n".into(),
        (false, _) => global_path(),
    };
    let prior = fs::read(&path).ok().and_then(|bytes| hash_of(&bytes));
    match write_entry(&path, name, &entry) {
        Ok(()) => {
            // The user typed this entry, not the whole file: an approval the
            // file already had survives, and one it never had is not invented.
            if path != global_path() {
                if let Some(dir) = cwd.as_deref() {
                    carry_approval(dir, &path, prior.as_deref());
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
        let prior = fs::read(&file).ok().and_then(|bytes| hash_of(&bytes));
        match remove_entry(&file, name) {
            Ok(true) => {
                removed += 1;
                if file != global_path() {
                    if let Some(dir) = cwd.as_deref() {
                        carry_approval(dir, &file, prior.as_deref());
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
        Some(dir) => {
            let message = forget_trust(&dir);
            // The record is gone, so the file is not approved any more: the
            // listing must not keep saying it is. The running servers stay
            // what they are until the next start.
            let mut reg = lock();
            reg.trusted = false;
            reg.recorded = false;
            format!("◦ {message}\n")
        }
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

/// The one place a file is replaced: beside it, then renamed, so a crash
/// leaves the file it had rather than half of a new one. The temp is a
/// sibling because a rename across filesystems is a copy.
fn write_atomic(path: &Path, text: &str, private: bool) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mcp.json".to_string());
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    if private {
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut file| {
            if private {
                // A temp left by an earlier crash may carry a looser mode; the
                // rename would install it.
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(text.as_bytes())
        })
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

fn write_config(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let text = format!("{text}\n");
    // The global file can carry a header value or an env value; the project
    // one is the repository's, and takes the mode a committed file has.
    let private = path == global_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let made = match private {
                true => state::private_dir(parent),
                false => fs::create_dir_all(parent),
            };
            made.map_err(|e| format!("{}: {e}", parent.display()))?;
        }
    }
    write_atomic(path, &text, private)
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

/// One message is one line, and a line over `LINE_CAP` stops the reader: the
/// server is still running, so this is not an exit and must not be reported as
/// one.
fn cap_reason() -> String {
    format!(
        "the reply was over the {} MiB line cap; restart axe to reload the server",
        LINE_CAP >> 20
    )
}

/// Char-safe cut, marked at the end: a description or a stderr tail longer
/// than the cap must not split a character.
fn cut(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(12)).collect();
    out.push_str(" …[truncated]");
    out
}

fn short(text: &str) -> String {
    cut(text.trim(), 400)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        "tool"
    } else {
        "tools"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(name: &str, command: &str) -> Entry {
        Entry {
            name: name.into(),
            command: Some(command.into()),
            url: None,
            args: Vec::new(),
            env: Vec::new(),
            headers: Vec::new(),
            vars: Vec::new(),
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
        assert_eq!(tool_name("a.b", "x/y", &HashSet::new()), "mcp__a_b__x_y");
        let long = tool_name("s", &"t".repeat(80), &HashSet::new());
        assert!(long.len() <= NAME_CAP, "{}", long.len());
        assert!(long.starts_with("mcp__s__t"));
    }

    #[test]
    fn references_are_found_and_resolved() {
        std::env::set_var("AXE_TEST_MCP_VAR", "one");
        assert_eq!(
            vars_in("a ${AXE_TEST_MCP_VAR} $AXE_TEST_MCP_VAR $$X"),
            vec!["AXE_TEST_MCP_VAR".to_string()]
        );
        assert_eq!(expand("a $AXE_TEST_MCP_VAR $$X").unwrap(), "a one $X");
        assert!(expand("$AXE_TEST_MCP_NOT_SET")
            .unwrap_err()
            .contains("not set"));
        assert_eq!(expand("$5").unwrap(), "$5");
        std::env::remove_var("AXE_TEST_MCP_VAR");
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
        assert_eq!(
            merged
                .iter()
                .find(|e| e.name == "files")
                .unwrap()
                .command
                .as_deref(),
            Some("other")
        );
        assert!(!merged.iter().find(|e| e.name == "git").unwrap().enabled);
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
            &json!({ "command": "npx", "args": ["-y", "s"],
                     "env": { "TOKEN": "${TOKEN}" } }),
            file,
            false,
        )
        .unwrap();
        assert_eq!(ok.args, vec!["-y".to_string(), "s".to_string()]);
        assert_eq!(ok.env, vec![("TOKEN".into(), "${TOKEN}".into())]);
        assert_eq!(ok.vars, vec!["TOKEN".to_string()]);

        // An array of names is the shorthand for a pass-through.
        let shorthand = parse_entry(
            "x",
            &json!({ "command": "c", "env": ["A_TOKEN"] }),
            file,
            false,
        )
        .unwrap();
        assert_eq!(shorthand.env, vec![("A_TOKEN".into(), "${A_TOKEN}".into())]);

        // Remote servers name headers, and take no env or args.
        let remote = parse_entry(
            "sentry",
            &json!({ "url": "https://mcp.sentry.dev/mcp",
                     "headers": { "Authorization": "Bearer ${KEY}" } }),
            file,
            false,
        )
        .unwrap();
        assert!(remote.remote());
        assert_eq!(remote.vars, vec!["KEY".to_string()]);
        assert!(parse_entry(
            "x",
            &json!({ "url": "https://e/mcp", "args": ["a"] }),
            file,
            false
        )
        .is_err());
        assert!(parse_entry(
            "x",
            &json!({ "command": "c", "headers": { "A": "B" } }),
            file,
            false
        )
        .is_err());
        assert!(parse_entry(
            "x",
            &json!({ "command": "c", "url": "https://e/mcp" }),
            file,
            false
        )
        .is_err());
        assert!(parse_entry("x", &json!({ "url": "ftp://e" }), file, false).is_err());

        // A field axe would ignore must not run half a server.
        let err =
            parse_entry("x", &json!({ "command": "c", "cwd": "/" }), file, false).unwrap_err();
        assert!(err.contains("unknown key"), "{err}");
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
        let path = rendered
            .rsplit_once("[full output: ")
            .and_then(|(_, rest)| rest.strip_suffix(']'))
            .unwrap();
        let saved = fs::read_to_string(path).unwrap();
        assert_eq!(saved.chars().count(), RESULT_CAP + 100);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn trust_is_by_path_and_content() {
        // The record lives under the state root, so the test needs a scratch
        // home: a sandboxed build has none the process may write.
        state::testenv::home("mcp-trust", |home| {
            let dir = home.join("repo");
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join(".mcp.json");
            fs::write(&file, "{}").unwrap();
            let Some(hash) = hash_of(b"{}") else {
                return; // no digester on this host: nothing can be approved
            };
            assert!(!approved(&dir, &file, &hash));
            remember(&dir, &file, &hash);
            assert!(approved(&dir, &file, &hash));
            let other = hash_of(b"{\"mcpServers\":{}}").expect("a digester");
            assert!(!approved(&dir, &file, &other));
            let mode = fs::metadata(trust_file(&dir)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the record is the user's");
            assert!(forget_trust(&dir).contains("forgotten"));
            assert!(!approved(&dir, &file, &hash));
        });
    }

    /// An add or a remove through axe keeps an approval the file already had,
    /// and never invents one for the entries the user did not see.
    #[test]
    fn an_axe_write_carries_only_an_approval_that_existed() {
        state::testenv::home("mcp-carry", |home| {
            let dir = home.join("repo");
            fs::create_dir_all(&dir).unwrap();
            let file = dir.join(".mcp.json");
            let prior = b"{\"mcpServers\":{\"a\":{\"command\":\"/bin/true\"}}}";
            fs::write(&file, prior).unwrap();
            let Some(prior_hash) = hash_of(prior) else {
                return; // no digester on this host: nothing can be approved
            };

            // Never approved: the new content stays unapproved, so the start
            // still asks about everything the file now holds.
            let added = b"{\"mcpServers\":{\"a\":{\"command\":\"/bin/true\"},\"b\":{\"command\":\"/bin/false\"}}}";
            fs::write(&file, added).unwrap();
            carry_approval(&dir, &file, Some(&prior_hash));
            let added_hash = hash_of(added).expect("a digester");
            assert!(!approved(&dir, &file, &added_hash), "nothing was blessed");

            // Approved before: the user's own edit keeps the approval.
            fs::write(&file, prior).unwrap();
            remember(&dir, &file, &prior_hash);
            fs::write(&file, added).unwrap();
            carry_approval(&dir, &file, Some(&prior_hash));
            assert!(approved(&dir, &file, &added_hash));
        });
    }

    /// The listing reports the record, not whether entries happen to be in
    /// force: after a forget, and for a file that never needed approval.
    /// `/mcp help` is where a user meets the flags, so every block it names
    /// must survive the string splicing it is written with.
    #[test]
    fn the_help_names_every_flag_and_its_meaning() {
        let help = empty_state();
        for wanted in [
            "--project",
            "--url",
            "--env NAME",
            "NAME=VALUE writes the value as given",
            "--header 'K: V'",
            "a url is streamable HTTP",
        ] {
            assert!(help.contains(wanted), "{wanted:?} missing from:\n{help}");
        }
    }

    #[test]
    fn the_approval_line_is_the_record() {
        assert_eq!(approval_state(true, true), "approved");
        assert_eq!(approval_state(true, false), "approved");
        assert_eq!(approval_state(false, true), "not approved");
        assert_eq!(approval_state(false, false), "nothing to approve");
        assert!(cap_reason().contains("4 MiB"), "{}", cap_reason());
    }

    /// A line over the cap stops the reader without pretending the server
    /// exited: the waiters get their answer and the flag says which happened.
    #[test]
    fn an_oversized_reply_is_the_cap_and_not_an_exit() {
        use std::io::Cursor;
        let pending: Arc<Mutex<HashMap<u64, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = std::sync::mpsc::channel();
        pending.lock().unwrap().insert(7, tx);
        let dead = Arc::new(AtomicBool::new(false));
        let over_cap = Arc::new(AtomicBool::new(false));
        let huge = vec![b'x'; LINE_CAP + 1];
        read_replies(
            Cursor::new(huge),
            pending.clone(),
            dead.clone(),
            over_cap.clone(),
        );
        assert!(dead.load(Ordering::SeqCst));
        assert!(over_cap.load(Ordering::SeqCst));
        assert_eq!(rx.recv().ok(), Some(Value::Null), "the waiter is released");
    }

    /// The approval question lists what the entry reads, so the command and
    /// its arguments are expanded from that environment at start.
    #[test]
    fn a_command_and_its_arguments_expand_from_the_environment() {
        std::env::set_var("AXE_TEST_MCP_ARG", "hello");
        let mut with_args = entry("t", "$AXE_TEST_MCP_COMMAND");
        with_args.args = vec![
            "--greeting".into(),
            "${AXE_TEST_MCP_ARG}".into(),
            "$$literal".into(),
        ];
        std::env::set_var("AXE_TEST_MCP_COMMAND", "tool");
        let (program, args) = stdio_words(&with_args).unwrap();
        assert_eq!(program, "tool");
        assert_eq!(args, ["--greeting", "hello", "$literal"]);

        // The question lists the same names the expansion reads.
        let parsed = parse_entry(
            "t",
            &json!({ "command": "tool", "args": ["--greeting", "${AXE_TEST_MCP_ARG}"] }),
            Path::new("/tmp/mcp.json"),
            false,
        )
        .unwrap();
        assert_eq!(parsed.vars, vec!["AXE_TEST_MCP_ARG".to_string()]);

        std::env::remove_var("AXE_TEST_MCP_ARG");
        let err = stdio_words(&with_args).unwrap_err();
        assert!(err.contains("args[1]"), "{err}");
        assert!(err.contains("AXE_TEST_MCP_ARG is not set"), "{err}");
        std::env::remove_var("AXE_TEST_MCP_COMMAND");
    }

    /// A replaced file is written beside itself and renamed: a crash leaves the
    /// old one, and the private modes stay private.
    #[test]
    fn a_write_lands_whole_and_leaves_no_temp_behind() {
        let dir = state::testenv::dir("mcp-atomic");
        let file = dir.join("mcp.json");
        write_atomic(&file, "{\"a\":1}\n", true).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"a\":1}\n");
        write_atomic(&file, "{\"b\":2}\n", true).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"b\":2}\n");
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(leftovers, vec![file.clone()], "{leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_trust_hash_is_a_sha256_of_the_bytes() {
        let Some(hash) = hash_of(b"hello") else {
            return; // the host's digester is the mechanism, like gzip
        };
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // The approval binds the content: one byte changes the sum, and no
        // suffix can be chosen to bring it back.
        assert_ne!(hash_of(b"hello").as_deref(), hash_of(b"hello ").as_deref());
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
        fs::write(dir.join(".mcp.json"), "{}").unwrap();
        assert_eq!(project_file(&nested), Some(repo.join(".mcp.json")));
        assert_eq!(project_target(&nested), repo.join(".mcp.json"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_flags_carry_a_project_a_url_and_variable_names() {
        let (project, env, headers, url, timeout, words) =
            add_flags("--env TOKEN github npx -y s").unwrap();
        assert!(!project);
        assert_eq!(env, vec![("TOKEN".to_string(), "${TOKEN}".to_string())]);
        assert!(headers.is_empty());
        assert!(url.is_none());
        assert_eq!(timeout, None);
        assert_eq!(words, vec!["github", "npx", "-y", "s"]);

        let (project, env, headers, url, timeout, words) = add_flags(
            "--project --url https://e/mcp --header 'Authorization: Bearer ${K}' \
             --env K --timeout 90 sentry",
        )
        .unwrap();
        assert!(project);
        assert_eq!(url.as_deref(), Some("https://e/mcp"));
        assert_eq!(env, vec![("K".to_string(), "${K}".to_string())]);
        assert_eq!(
            headers,
            vec![("Authorization".to_string(), "Bearer ${K}".to_string())]
        );
        assert_eq!(timeout, Some(90));
        assert_eq!(words, vec!["sentry"]);

        assert!(add_flags("--env").is_err());
        assert!(add_flags("--env 1BAD x c").is_err());
        assert!(add_flags("--header 'no colon' x c").is_err());
        assert!(add_flags("--timeout 0 x c").is_err());
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

    /// A stdio server in one line of shell: initialize, list, call.
    fn stdio_fixture(dir: &Path) -> Vec<String> {
        let script = r#"
read line
echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t","version":"1"}}}'
read line
read line
echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"say it back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}}'
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"said it back\"}]}}"
done
"#;
        let file = dir.join("server.sh");
        fs::write(&file, script).unwrap();
        vec![file.display().to_string()]
    }

    #[test]
    fn a_stdio_server_is_asked_for_its_tools_and_answers_a_call() {
        let dir = state::testenv::dir("mcp-server");
        let args = stdio_fixture(&dir);
        let mut e = entry("t", "/bin/sh");
        e.args = args;
        let server = Server::open(&e, false).expect("a server that answers");
        assert_eq!(server.listed.len(), 1);
        assert_eq!(server.listed[0].original, "echo");
        let mut running = vec![Some(server)];
        name_tools(&mut running);
        let server = running[0].as_mut().unwrap();
        assert_eq!(server.tools[0].exposed, "mcp__t__echo");
        assert_eq!(
            server.call("echo", &json!({ "text": "hi" }), &Cancel::new()),
            "said it back"
        );
        server.stop();
        assert!(server
            .call("echo", &json!({}), &Cancel::new())
            .contains("error: mcp t"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stdio_server_answers_an_interrupt_and_stays_up() {
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
        assert_eq!(server.call("slow", &json!({}), &Cancel::new()), "late");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_poisoned_registry_still_stops_its_servers() {
        // The environmental lock serialises this with the one test that reads
        // the process-wide registry for its emptiness.
        state::testenv::home("mcp-poison", |home| {
            let dir = home.join("srv");
            fs::create_dir_all(&dir).unwrap();
            let args = stdio_fixture(&dir);
            let mut e = entry("t", "/bin/sh");
            e.args = args;
            let server = Server::open(&e, false).expect("a server that answers");
            {
                let mut reg = lock();
                reg.running.push(Some(server));
            }

            // A panic inside the lock leaves it poisoned, which is what a
            // panicking call would do; cleanup must still reach the servers.
            let _ = std::panic::catch_unwind(|| {
                let _held = lock();
                panic!("poison the registry");
            });
            assert!(registry().lock().is_err(), "the lock is poisoned");

            shutdown();
            let mut reg = lock();
            let mut server = reg.running.pop().flatten().expect("the server is ours");
            drop(reg);
            match &mut server.conn {
                Conn::Stdio(conn) => assert!(
                    conn.child.try_wait().ok().flatten().is_some(),
                    "the server was stopped"
                ),
                Conn::Http(_) => panic!("the fixture is stdio"),
            }
        });
    }

    /// A mock remote server: JSON for initialize and calls, SSE for the tool
    /// list, a session id on every reply, and a 401 until a bearer arrives.
    fn mock_remote(require_auth: bool) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).ok();
                let mut length = 0usize;
                let mut auth = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).ok();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            auth = value.trim().starts_with("Bearer ");
                        }
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).ok();
                let body = String::from_utf8_lossy(&body).to_string();
                let (status, content_type, payload) = if require_auth && !auth {
                    (
                        401,
                        "application/json",
                        json!({ "error": "unauthorized" }).to_string(),
                    )
                } else if body.contains("\"initialize\"") {
                    (
                        200,
                        "application/json",
                        json!({ "jsonrpc": "2.0", "id": 1, "result": {
                            "protocolVersion": "2025-06-18", "capabilities": {},
                            "serverInfo": { "name": "t", "version": "1" } } })
                        .to_string(),
                    )
                } else if body.contains("\"tools/list\"") {
                    let message = json!({ "jsonrpc": "2.0", "id": 2, "result": {
                        "tools": [{ "name": "echo", "description": "say it back",
                                    "inputSchema": { "type": "object" } }] } });
                    (
                        200,
                        "text/event-stream",
                        format!("event: message\ndata: {message}\n\n"),
                    )
                } else if body.contains("\"tools/call\"") {
                    let id = body
                        .split("\"id\":")
                        .nth(1)
                        .and_then(|rest| rest.split(',').next())
                        .and_then(|n| n.trim().parse::<u64>().ok())
                        .unwrap_or(3);
                    (
                        200,
                        "application/json",
                        json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": [{ "type": "text", "text": "said it back" }] } })
                        .to_string(),
                    )
                } else {
                    (400, "application/json", "{}".to_string())
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\n\
                     Mcp-Session-Id: s1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.flush();
            }
        });
        url
    }

    #[test]
    fn a_remote_server_is_listed_over_sse_and_answers_over_json() {
        let url = mock_remote(false);
        let mut e = entry("r", "");
        e.command = None;
        e.url = Some(url);
        let server = Server::open(&e, false).expect("a remote server");
        assert_eq!(server.listed.len(), 1);
        let mut running = vec![Some(server)];
        name_tools(&mut running);
        let server = running[0].as_mut().unwrap();
        assert_eq!(
            server.call("echo", &json!({}), &Cancel::new()),
            "said it back"
        );
    }

    #[test]
    fn a_remote_server_gets_the_bearer_its_headers_name() {
        let url = mock_remote(true);
        let mut e = entry("r", "");
        e.command = None;
        e.url = Some(url);
        let err = Server::open(&e, false).err().expect("401 is a failure");
        assert!(err.contains("401"), "{err}");

        let url = mock_remote(true);
        let mut e = entry("r", "");
        e.command = None;
        e.url = Some(url);
        e.headers = vec![("Authorization".into(), "Bearer at".into())];
        let server = Server::open(&e, false).expect("the header is sent");
        assert_eq!(server.listed.len(), 1);
    }

    #[test]
    fn explore_mode_confines_a_servers_writes() {
        let dir = state::testenv::dir("mcp-confined");
        let target = dir.join("out");
        // The server writes as it starts: the kernel decides before any call,
        // so the test is the spawn, not a tool.
        let fixture = |path: &Path| -> String {
            format!(
                r#"
printf x > {} 2>/dev/null
read line
echo '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-06-18","capabilities":{{}},"serverInfo":{{"name":"t","version":"1"}}}}}}'
read line
read line
echo '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}'
while read line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"content\":[]}}}}"
done
"#,
                path.display()
            )
        };
        let run = |explore: bool| -> bool {
            let dir = state::testenv::dir(&format!("mcp-write-{explore}"));
            let file = dir.join("server.sh");
            fs::write(&file, fixture(&target)).unwrap();
            let mut e = entry("t", "/bin/sh");
            e.args = vec![file.display().to_string()];
            let server = Server::open(&e, explore);
            assert!(server.is_ok(), "the server starts: {:?}", server.err());
            let _ = fs::remove_dir_all(&dir);
            target.exists()
        };
        assert!(!run(true), "explore mode denies the write");
        assert!(!target.exists(), "nothing was written");
        assert!(run(false), "normal mode writes");
        assert!(target.exists());
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
            assert!(listing.contains("global: every directory"), "{listing}");
            assert!(
                listing.contains("project: this repository only"),
                "{listing}"
            );
            assert!(listing.contains("--url"), "{listing}");
            shutdown();
        });
        let _ = fs::remove_dir_all(&dir);
    }
}
