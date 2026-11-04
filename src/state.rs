//! The per-directory state file: the cwd key, load, and an atomic write.
//! Sessions live under the same project directory.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// What nano remembers for one working directory: the selection, and how much
/// of the reasoning is drawn. State is written, config is read — nothing here
/// ever touches `~/.config/nano/config.json`.
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

/// `$XDG_STATE_HOME` or `$HOME/.local/state`, then `nano`.
fn root() -> PathBuf {
    if let Ok(dir) = env::var("XDG_STATE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("nano");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/state/nano")
}

/// The cwd as one path component: every byte outside `[A-Za-z0-9._-]` written
/// as `%XX`, so `/home/anon/nano` becomes `%2Fhome%2Fanon%2Fnano`. Reversible,
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

fn fnv1a(bytes: &[u8]) -> u64 {
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

/// `<state>/nano/projects/<key>/state.json`.
fn file(cwd: &Path) -> PathBuf {
    project(cwd).join("state.json")
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
            eprintln!("nano: cannot read {}: {e}", file.display());
            return state;
        }
    };
    let v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nano: cannot parse {}: {e}", file.display());
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
        fs::create_dir_all(dir)
            .map_err(|e| format!("cannot save state to {}: {e}", file.display()))?;
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
        fs::write(&tmp, text)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_reversible_and_safe() {
        assert_eq!(key(Path::new("/home/anon/nano")), "%2Fhome%2Fanon%2Fnano");
        assert!(!key(Path::new("/a/../b")).contains('/'));
    }
}
