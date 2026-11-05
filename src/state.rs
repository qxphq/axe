//! The per-directory state file: the cwd key, load, and an atomic write.
//! Sessions live under the same project directory.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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
    /// Project skills are trusted (`/trust`).
    pub trusted: bool,
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

/// `<state>/axe/projects/<key>/state.json`.
fn file(cwd: &Path) -> PathBuf {
    project(cwd).join("state.json")
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
    state.trusted = v["trusted"].as_bool().unwrap_or(false);
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
            "trusted": self.trusted,
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
}
