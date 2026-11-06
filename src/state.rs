//! The per-directory state file: the cwd key, load, and an atomic write, plus
//! the global default model one level up. Sessions live under the same project
//! directory.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// What axe remembers for one working directory: the selection, and how much
/// of the reasoning is drawn. State is written, config is read — nothing here
/// ever touches `~/.config/axe/config.json`.
#[derive(Default)]
pub struct State {
    /// The directory this belongs to. Empty when `current_dir` failed, which
    /// `save` treats as "nowhere to write".
    pub cwd: PathBuf,
    /// `provider/id`, the last model picked here.
    pub model: Option<String>,
    /// Model spec (`provider/id`) -> remembered thinking level.
    pub thinking: Vec<(String, String)>,
    /// Show reasoning in the transcript (`/thoughts`).
    pub thoughts: bool,
}

/// `$XDG_STATE_HOME` or `$HOME/.local/state`, then `axe`.
fn root() -> PathBuf {
    if let Ok(dir) = env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("axe");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/axe")
}

/// The cwd as one path component: every byte outside `[A-Za-z0-9._-]` written
/// as `%XX`, so `/home/anon/axe` becomes `%2Fhome%2Fanon%2Faxe`. Reversible,
/// and injective because `%` is escaped too.
fn key(cwd: &Path) -> String {
    let path = cwd.to_string_lossy();
    let mut key = String::with_capacity(path.len() * 2);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            key.push(byte as char);
        } else {
            key.push_str(&format!("%{byte:02X}"));
        }
    }
    // A component is capped at 255 bytes. Past 200 the key is cut and tagged
    // with its hash: a deep path degrades into a hash instead of a failed save.
    if key.len() > 200 {
        key.truncate(160);
        key.push_str(&format!("-{:016x}", fnv1a(path.as_bytes())));
    }
    key
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The directory that belongs to one working directory: its state file and
/// its sessions. Sessions are siblings of `state.json`, under `sessions/`.
pub fn project(cwd: &Path) -> PathBuf {
    root().join("projects").join(key(cwd))
}

/// `<state>/axe/projects/<key>/state.json`.
fn file(cwd: &Path) -> PathBuf {
    project(cwd).join("state.json")
}

/// `<state>/axe/default.json`: the selection a directory that pins none starts
/// on. It lives here, not in the config, because the config is read-only input.
fn default_file() -> PathBuf {
    root().join("default.json")
}

/// The global default model, `provider/id`. Best effort: a missing or broken
/// file is no default, never a failed start.
pub fn load_default() -> Option<String> {
    let file = default_file();
    let raw = fs::read_to_string(&file).ok()?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        Err(e) => {
            eprintln!("axe: cannot parse {}: {e}", file.display());
            None
        }
    }
}

/// Write the global default whole, through a temporary like `State::save`.
pub fn save_default(spec: &str) -> Result<(), String> {
    let file = default_file();
    let Some(dir) = file.parent() else {
        return Ok(());
    };
    private_dir(dir).map_err(|e| format!("cannot save the default model: {e}"))?;
    let value = serde_json::json!({ "model": spec });
    let text = serde_json::to_string_pretty(&value)
        .map(|text| format!("{text}\n"))
        .map_err(|e| format!("cannot save the default model: {e}"))?;
    let tmp = file.with_extension("json.tmp");
    write_private(&tmp, &text).map_err(|e| format!("cannot save the default model: {e}"))?;
    fs::rename(&tmp, &file).map_err(|e| format!("cannot save the default model: {e}"))
}

/// Prompt lines remembered for this directory.
pub const HISTORY: usize = 500;

/// A history file over this size is cut back to its last `HISTORY` entries the
/// next time it is read.
const HISTORY_BYTES: u64 = 128 * 1024;

/// `<state>/axe/projects/<key>/history.jsonl`: the prompt lines typed here, one
/// JSON string per line so a pasted multi-line prompt survives a round trip.
pub fn history(cwd: &Path) -> PathBuf {
    project(cwd).join("history.jsonl")
}

/// The remembered prompt lines, oldest first. A file that cannot be read is an
/// empty history: remembering prompts must never fail a start.
pub fn read_history(cwd: &Path) -> Vec<String> {
    if cwd.as_os_str().is_empty() {
        return Vec::new();
    }
    read_history_file(&history(cwd))
}

/// Remember one submitted line. Best effort at the call site: a prompt that
/// cannot be remembered is not a failed prompt.
pub fn append_history(cwd: &Path, line: &str) -> Result<(), String> {
    if cwd.as_os_str().is_empty() {
        return Ok(());
    }
    append_history_file(&history(cwd), line)
}

fn read_history_file(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines = parse_history(&text);
    if lines.len() > HISTORY {
        lines.drain(..lines.len() - HISTORY);
    }
    if text.len() as u64 > HISTORY_BYTES {
        let _ = write_history_file(path, &lines);
    }
    lines
}

fn parse_history(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|line| !line.trim().is_empty())
        .collect()
}

fn append_history_file(path: &Path, line: &str) -> Result<(), String> {
    let line = line.trim_end();
    // A leading space is the shell's "do not remember this", and a repeat of
    // the line before it is not a new step.
    if line.trim().is_empty() || line.starts_with(' ') {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    if newest_line(path).as_deref() == Some(line) {
        return Ok(());
    }
    let encoded = serde_json::to_string(line).map_err(|e| format!("encode history: {e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    set_mode(path, 0o600).map_err(|e| format!("chmod {}: {e}", path.display()))?;
    file.write_all(format!("{encoded}\n").as_bytes())
        .map_err(|e| format!("append history: {e}"))
}

/// Bytes of the history file's tail read to find its newest line. The check
/// runs on every submitted prompt, so the whole file is read only when the
/// window cannot answer.
const TAIL: u64 = 8 * 1024;

/// The newest entry the file holds, decoded. `None` when it cannot be read: a
/// prompt that cannot be compared is appended, not lost.
fn newest_line(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes).ok()?;
    // The window may begin inside a line, so its first line is not a line; the
    // last one is, unless the newest entry is longer than the window, and then
    // the whole file is the honest answer.
    let text = String::from_utf8_lossy(&bytes);
    let last = text.rsplit('\n').find(|line| !line.trim().is_empty())?;
    match serde_json::from_str::<String>(last) {
        Ok(line) => Some(line),
        Err(_) if start > 0 => {
            let all = fs::read_to_string(path).ok()?;
            parse_history(&all).pop()
        }
        Err(_) => None,
    }
}

/// Replace the file with `lines`, atomically: a crash leaves the old history,
/// not half of a new one.
fn write_history_file(path: &Path, lines: &[String]) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut text = String::new();
    for line in lines {
        if let Ok(encoded) = serde_json::to_string(line) {
            text.push_str(&encoded);
            text.push('\n');
        }
    }
    write_private(&tmp, &text)?;
    fs::rename(&tmp, path)
}

/// Create `dir` and the axe-owned directories above it, all `0700`: a
/// directory's state and its sessions are private to the user.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    let root = root();
    let mut at = dir.parent();
    while let Some(parent) = at.filter(|parent| parent.starts_with(&root)) {
        set_mode(parent, 0o700)?;
        at = parent.parent();
    }
    Ok(())
}

/// Create (or truncate) a file only its owner can read. The mode is forced
/// after opening, so a temporary left by an older version is tightened too.
pub fn create_private(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    set_mode(path, 0o600)?;
    Ok(file)
}

/// Write a whole file with `create_private`'s mode.
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    create_private(path)?.write_all(text.as_bytes())
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Best-effort: a missing file is silent, a malformed one is a warning and a
/// fresh start. Nothing here is worth failing a run over.
pub fn load(cwd: &Path) -> State {
    let mut state = State {
        cwd: cwd.to_path_buf(),
        ..State::default()
    };
    let file = file(cwd);
    let raw = match fs::read_to_string(&file) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return state,
        Err(e) => {
            eprintln!("axe: cannot read {}: {e}", file.display());
            return state;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("axe: cannot parse {}: {e}", file.display());
            return state;
        }
    };
    state.model = v["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    state.thinking = v["thinking"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, l)| Some((k.clone(), l.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    state.thoughts = v["thoughts"].as_bool().unwrap_or(false);
    state
}

impl State {
    /// Rewrite the file whole, through a temporary: a kill mid-write leaves the
    /// previous state intact. Two instances in one directory are
    /// last-write-wins; there is no lock.
    pub fn save(&self) -> Result<(), String> {
        if self.cwd.as_os_str().is_empty() {
            return Ok(());
        }
        let file = file(&self.cwd);
        let Some(dir) = file.parent() else {
            return Ok(());
        };
        private_dir(dir).map_err(|e| format!("cannot save state to {}: {e}", file.display()))?;
        let thinking: serde_json::Map<String, Value> = self
            .thinking
            .iter()
            .map(|(spec, level)| (spec.clone(), Value::String(level.clone())))
            .collect();
        let value = serde_json::json!({
            "cwd": self.cwd.display().to_string(),
            "model": self.model,
            "thinking": thinking,
            "thoughts": self.thoughts,
        });
        let text = serde_json::to_string_pretty(&value)
            .map(|text| format!("{text}\n"))
            .map_err(|e| format!("cannot save state to {}: {e}", file.display()))?;
        let tmp = file.with_extension("json.tmp");
        write_private(&tmp, &text)
            .map_err(|e| format!("cannot save state to {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &file).map_err(|e| format!("cannot save state to {}: {e}", file.display()))
    }

    pub fn thinking_for(&self, spec: &str) -> Option<String> {
        self.thinking
            .iter()
            .find(|(s, _)| s == spec)
            .map(|(_, level)| level.clone())
    }

    pub fn set_model(&mut self, spec: &str) {
        self.model = Some(spec.to_string());
    }

    pub fn set_thinking(&mut self, spec: &str, level: &str) {
        match self.thinking.iter_mut().find(|(s, _)| s == spec) {
            Some((_, remembered)) => *remembered = level.to_string(),
            None => self.thinking.push((spec.to_string(), level.to_string())),
        }
    }
}

/// A scratch home for the tests that read the environment. The state, config
/// and skills paths all derive from `HOME`, `XDG_STATE_HOME` and
/// `XDG_CONFIG_HOME`, and the environment is the process's: the tests that
/// change it run one at a time under this lock, and everything they write lands
/// inside the directory and is removed with it.
#[cfg(test)]
pub(crate) mod testenv {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// A directory named for the test, emptied first.
    pub fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("axe-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Run `f` with a scratch home, a scratch state root and a scratch config
    /// root, so a test's writes land there and nowhere else.
    pub fn home<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _held = lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = dir(name);
        let saved: Vec<(&str, Option<OsString>)> = ["HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|var| (*var, std::env::var_os(var)))
            .collect();
        std::env::set_var("HOME", &dir);
        std::env::set_var("XDG_STATE_HOME", dir.join("state"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        let out = f(&dir);
        for (var, value) in saved {
            match value {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_reversible_and_safe() {
        assert_eq!(key(Path::new("/home/anon/axe")), "%2Fhome%2Fanon%2Faxe");
        assert!(!key(Path::new("/a/../b")).contains('/'));
    }

    #[test]
    fn selection_and_thinking_roundtrip() {
        let mut state = State::default();
        state.set_model("prov/model");
        state.set_thinking("prov/model", "high");
        assert_eq!(state.model.as_deref(), Some("prov/model"));
        assert_eq!(state.thinking_for("prov/model").as_deref(), Some("high"));
        state.set_thinking("prov/model", "low");
        assert_eq!(state.thinking_for("prov/model").as_deref(), Some("low"));
    }

    #[test]
    fn fnv_hash_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn history_appends_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("axe-test-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("history.jsonl");
        append_history_file(&path, "first").unwrap();
        append_history_file(&path, "second\nline").unwrap();
        // A repeat of the newest, a leading space, and a blank are not steps.
        append_history_file(&path, "second\nline").unwrap();
        append_history_file(&path, " leading").unwrap();
        append_history_file(&path, "   ").unwrap();
        assert_eq!(
            read_history_file(&path),
            vec!["first".to_string(), "second\nline".to_string()]
        );
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        // A line that is not a JSON string is skipped, not fatal.
        fs::write(&path, "{\"a\"}\n\"kept\"\n").unwrap();
        assert_eq!(read_history_file(&path), vec!["kept".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_newest_line_is_read_from_the_tail() {
        let dir = std::env::temp_dir().join(format!("axe-test-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        assert!(newest_line(&path).is_none(), "a missing file is no line");
        for i in 0..2_000 {
            append_history_file(&path, &format!("entry {i}")).unwrap();
        }
        assert_eq!(newest_line(&path).as_deref(), Some("entry 1999"));
        // A newest entry larger than the window is read whole, not truncated
        // into a suffix that would append a duplicate.
        let long = "x".repeat(20_000);
        append_history_file(&path, &long).unwrap();
        assert_eq!(newest_line(&path).as_deref(), Some(long.as_str()));
        // A damaged tail decodes to nothing rather than to a wrong line.
        fs::write(&path, "\"kept\"\nnot json").unwrap();
        assert!(newest_line(&path).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_capped_and_pruned() {
        let dir = std::env::temp_dir().join(format!("axe-test-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let mut text = String::new();
        for i in 0..20_000 {
            text.push_str(&format!(
                "\"entry {i} with some padding so the file is over the cap\"\n"
            ));
        }
        fs::write(&path, &text).unwrap();
        let lines = read_history_file(&path);
        assert_eq!(lines.len(), HISTORY);
        assert_eq!(
            lines.last().unwrap(),
            "entry 19999 with some padding so the file is over the cap"
        );
        // The read pruned the file it read.
        assert!(fs::metadata(&path).unwrap().len() < HISTORY_BYTES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_global_default_roundtrips_and_a_broken_one_is_none() {
        testenv::home("default-model", |home| {
            assert!(load_default().is_none(), "no file is no default");
            save_default("p/m").unwrap();
            assert_eq!(load_default().as_deref(), Some("p/m"));
            let mode =
                |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&default_file()), 0o600);
            assert_eq!(mode(&home.join("state/axe")), 0o700);

            fs::write(default_file(), "{ not json").unwrap();
            assert!(load_default().is_none());
            // An empty spec is no default either.
            fs::write(default_file(), r#"{"model": ""}"#).unwrap();
            assert!(load_default().is_none());
        });
    }

    #[test]
    fn private_files_have_private_modes() {
        let dir = std::env::temp_dir().join(format!("axe-test-private-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        private_dir(&dir).unwrap();
        let file = dir.join("state.json");
        write_private(&file, "x").unwrap();
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_state_file_roundtrips_under_the_state_home() {
        testenv::home("state-roundtrip", |home| {
            let cwd = home.join("work");
            fs::create_dir_all(&cwd).unwrap();
            let mut state = State {
                cwd: cwd.clone(),
                ..State::default()
            };
            state.set_model("prov/model");
            state.set_thinking("prov/model", "high");
            state.thoughts = true;
            state.save().unwrap();

            let loaded = load(&cwd);
            assert_eq!(loaded.model.as_deref(), Some("prov/model"));
            assert_eq!(loaded.thinking_for("prov/model").as_deref(), Some("high"));
            assert!(loaded.thoughts);
            assert_eq!(loaded.cwd, cwd);

            let mode =
                |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&project(&cwd)), 0o700);
            assert_eq!(mode(&file(&cwd)), 0o600);
        });
    }

    #[test]
    fn a_missing_or_broken_state_file_is_a_fresh_start() {
        testenv::home("state-broken", |home| {
            let cwd = home.join("work");
            let fresh = load(&cwd);
            assert!(fresh.model.is_none() && fresh.thinking.is_empty() && !fresh.thoughts);

            fs::create_dir_all(project(&cwd)).unwrap();
            fs::write(file(&cwd), "{ not json").unwrap();
            assert!(load(&cwd).model.is_none());

            // Fields that are absent keep their defaults; what is there is read.
            fs::write(
                file(&cwd),
                r#"{"thoughts": true, "model": "", "thinking": {"a/b": "low", "c/d": 3}}"#,
            )
            .unwrap();
            let loaded = load(&cwd);
            assert!(loaded.model.is_none(), "an empty model is no model");
            assert!(loaded.thoughts);
            assert_eq!(loaded.thinking_for("a/b").as_deref(), Some("low"));
            assert!(loaded.thinking_for("c/d").is_none(), "a level is a string");
        });
    }

    #[test]
    fn saving_nowhere_is_a_no_op() {
        assert!(State::default().save().is_ok());
    }

    #[test]
    fn the_prompt_history_lives_under_the_project() {
        testenv::home("state-history", |home| {
            let cwd = home.join("work");
            fs::create_dir_all(&cwd).unwrap();
            assert!(read_history(&cwd).is_empty());
            append_history(&cwd, "first").unwrap();
            append_history(&cwd, "second").unwrap();
            append_history(&cwd, "second").unwrap();
            assert_eq!(
                read_history(&cwd),
                vec!["first".to_string(), "second".to_string()]
            );
            assert!(history(&cwd).is_file());
            // An empty cwd remembers nothing and reads nothing.
            assert!(append_history(Path::new(""), "x").is_ok());
            assert!(read_history(Path::new("")).is_empty());
        });
    }

    #[test]
    fn a_long_key_becomes_a_hash() {
        let deep = Path::new("/x").join("y".repeat(400));
        let long = key(&deep);
        assert!(long.len() <= 200, "{long}");
        assert!(!long.contains('/'));
        // The same path always keys the same directory.
        assert_eq!(long, key(&deep));
        assert_ne!(long, key(&Path::new("/x").join("z".repeat(400))));
    }
}
