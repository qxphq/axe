//! Sessions: what one working directory remembers across runs.
//!
//! One directory per project holds a meta file and an append-only log for each
//! session. The meta is rewritten as the title and count change; the log is
//! never rewritten, so a crash can only leave an unfinished tail line. Old
//! bodies are gzipped in place by the host's `gzip`: the meta stays readable,
//! so listings and search keep working without unpacking anything, and a host
//! without gzip leaves the bodies plain.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::state;

/// Bodies kept plain. Older ones are compressed on startup: they still list
/// and answer searches through title and summary.
pub const KEEP: usize = 30;

pub const VERSION: u64 = 1;

/// Everything needed to list a session without reading its body.
#[derive(Clone)]
pub struct Meta {
    pub id: String,
    pub created: u64,
    pub updated: u64,
    pub title: String,
    pub summary: String,
    pub model: String,
    pub messages: u64,
    pub archived: bool,
}

/// `<state>/projects/<key>/sessions`.
pub fn dir(cwd: &Path) -> PathBuf {
    state::project(cwd).join("sessions")
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn log_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn gz_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl.gz"))
}

/// Ids name files, so only the shape `session_id` produces may pass.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Meta {
    pub fn new(id: &str, model: &str) -> Meta {
        let now = now();
        Meta {
            id: id.to_string(),
            created: now,
            updated: now,
            title: String::new(),
            summary: String::new(),
            model: model.to_string(),
            messages: 0,
            archived: false,
        }
    }

    pub fn load(dir: &Path, id: &str) -> Result<Meta, String> {
        if !valid_id(id) {
            return Err(format!("not a session id: {id}"));
        }
        let file = meta_path(dir, id);
        let raw = fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
        let v: Value =
            serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", file.display()))?;
        if v["v"].as_u64().unwrap_or(0) > VERSION {
            return Err(format!("session {} uses a newer format", id));
        }
        let text = |key: &str| v[key].as_str().unwrap_or("").to_string();
        Ok(Meta {
            // The filename is the identity; a sidecar cannot point elsewhere.
            id: id.to_string(),
            created: v["created"].as_u64().unwrap_or(0),
            updated: v["updated"].as_u64().unwrap_or(0),
            title: text("title"),
            summary: text("summary"),
            model: text("model"),
            messages: v["messages"].as_u64().unwrap_or(0),
            archived: v["archived"].as_bool().unwrap_or(false),
        })
    }

    /// Rewrite the sidecar through a temporary, like `state.json`.
    pub fn save(&self, dir: &Path) -> Result<(), String> {
        state::private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let file = meta_path(dir, &self.id);
        let value = json!({
            "v": VERSION,
            "id": self.id,
            "created": self.created,
            "updated": self.updated,
            "title": self.title,
            "summary": self.summary,
            "model": self.model,
            "messages": self.messages,
            "archived": self.archived,
        });
        let text = serde_json::to_string_pretty(&value)
            .map_err(|e| format!("encode {}: {e}", file.display()))?;
        let tmp = dir.join(format!("{}.json.tmp", self.id));
        state::write_private(&tmp, &format!("{text}\n"))
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &file).map_err(|e| format!("write {}: {e}", file.display()))
    }
}

/// Every readable meta, newest activity first. A malformed sidecar is skipped:
/// one broken session must not hide the others.
pub fn list(cwd: &Path) -> Vec<Meta> {
    let dir = dir(cwd);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut metas = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        if let Ok(meta) = Meta::load(&dir, id) {
            metas.push(meta);
        }
    }
    metas.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| b.created.cmp(&a.created))
            .then_with(|| b.id.cmp(&a.id))
    });
    metas
}

/// The open append-only body. One handle per active session.
pub struct Log {
    file: File,
}

impl Log {
    pub fn open(dir: &Path, id: &str) -> Result<Log, String> {
        state::private_dir(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let path = log_path(dir, id);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        // A body created before the mode was set stays readable by other
        // local users until it is tightened here.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        Ok(Log { file })
    }

    /// One API message per line, the same object the request carries.
    pub fn append(&mut self, message: &Value) -> Result<(), String> {
        let mut line =
            serde_json::to_string(message).map_err(|e| format!("encode message: {e}"))?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|e| format!("append session: {e}"))
    }
}

/// Read a body, unpacking it first when it was archived. The plain file is
/// left behind: resuming makes the session live again.
pub fn load_messages(
    dir: &Path,
    meta: &mut Meta,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>, String> {
    let plain = log_path(dir, &meta.id);
    let gz = gz_path(dir, &meta.id);
    if !plain.exists() {
        if gz.exists() {
            unpack(dir, meta, &gz, &plain)?;
        } else {
            return Err(format!("session {} has no body", meta.id));
        }
    } else if gz.exists() {
        // A crash after unpacking left the compressed copy behind.
        let _ = fs::remove_file(&gz);
    }
    if meta.archived {
        meta.archived = false;
        meta.save(dir)?;
    }

    let file = File::open(&plain).map_err(|e| format!("open {}: {e}", plain.display()))?;
    let mut messages = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                warnings.push(format!("read {}: {e}", plain.display()));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            // A kill mid-append leaves one partial line, always last.
            Err(_) => {
                warnings.push(format!("skipping a damaged line in {}", plain.display()));
                break;
            }
        };
        match message["role"].as_str() {
            Some("user" | "assistant" | "tool") => messages.push(message),
            _ => continue,
        }
    }
    // A killed append can leave the sidecar count one behind; the body is the
    // truth once it is read.
    if meta.messages != messages.len() as u64 {
        meta.messages = messages.len() as u64;
        let _ = meta.save(dir);
    }
    Ok(messages)
}

fn unpack(dir: &Path, meta: &mut Meta, gz: &Path, plain: &Path) -> Result<(), String> {
    let tmp = dir.join(format!("{}.jsonl.tmp", meta.id));
    gzip(&[OsStr::new("-dc"), gz.as_os_str()], &tmp)?;
    fs::rename(&tmp, plain).map_err(|e| format!("write {}: {e}", plain.display()))?;
    let _ = fs::remove_file(gz);
    meta.archived = false;
    meta.save(dir)?;
    Ok(())
}

/// Compress the bodies older than `keep`. Best-effort: a failure is reported
/// and the next startup tries again.
pub fn archive_old(cwd: &Path, keep: usize) {
    let dir = dir(cwd);
    let old: Vec<Meta> = list(cwd).into_iter().skip(keep).collect();
    if old.is_empty() {
        return;
    }
    if !have_gzip() {
        eprintln!("axe: gzip not found: old session bodies stay plain");
        return;
    }
    for mut meta in old {
        if meta.archived {
            continue;
        }
        let plain = log_path(&dir, &meta.id);
        let gz = gz_path(&dir, &meta.id);
        if !plain.exists() {
            if gz.exists() {
                meta.archived = true;
                if let Err(e) = meta.save(&dir) {
                    eprintln!("axe: cannot archive session {}: {e}", meta.id);
                }
            }
            continue;
        }
        if let Err(e) = compress(&dir, &meta, &plain, &gz) {
            eprintln!("axe: cannot archive session {}: {e}", meta.id);
            let _ = fs::remove_file(dir.join(format!("{}.jsonl.gz.tmp", meta.id)));
            continue;
        }
        meta.archived = true;
        if let Err(e) = meta.save(&dir) {
            eprintln!("axe: cannot archive session {}: {e}", meta.id);
        }
    }
}

fn compress(dir: &Path, meta: &Meta, plain: &Path, gz: &Path) -> Result<(), String> {
    if gz.exists() {
        // A crash after the rename but before the plain copy was removed.
        fs::remove_file(plain).map_err(|e| format!("remove {}: {e}", plain.display()))?;
        return Ok(());
    }
    let tmp = dir.join(format!("{}.jsonl.gz.tmp", meta.id));
    gzip(&[OsStr::new("-nc"), plain.as_os_str()], &tmp)?;
    fs::rename(&tmp, gz).map_err(|e| format!("write {}: {e}", gz.display()))?;
    fs::remove_file(plain).map_err(|e| format!("remove {}: {e}", plain.display()))?;
    Ok(())
}

/// Compression is the host's: `gzip` is essential on Debian, in every BSD base
/// and shipped by macOS, so nothing is vendored for it. A machine without it
/// keeps bodies plain instead of failing a startup over compression.
fn have_gzip() -> bool {
    Command::new("gzip")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Run `gzip` with stdout on `out`. stderr is captured: the REPL owns the
/// terminal, so a child writing to it would be a rendering bug. A failed run
/// leaves no partial file behind.
fn gzip(args: &[&OsStr], out: &Path) -> Result<(), String> {
    let file = state::create_private(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let err = match Command::new("gzip")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .output()
    {
        Ok(done) if done.status.success() => return Ok(()),
        Ok(done) => {
            let err = String::from_utf8_lossy(&done.stderr).trim().to_string();
            if err.is_empty() {
                format!("gzip: {}", done.status)
            } else {
                format!("gzip: {err}")
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => "gzip not found".to_string(),
        Err(e) => format!("gzip: {e}"),
    };
    let _ = fs::remove_file(out);
    Err(err)
}

/// One search hit: the session's position in the caller's slice, and a line
/// from the place that matched.
pub struct Hit {
    pub index: usize,
    pub snippet: String,
}

/// Every session containing all query terms, case-insensitively. Title and
/// summary are always searched; a plain body too. `metas` keeps its order.
pub fn search(cwd: &Path, metas: &[Meta], query: &str) -> Vec<Hit> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return metas
            .iter()
            .enumerate()
            .map(|(index, _)| Hit {
                index,
                snippet: String::new(),
            })
            .collect();
    }

    let dir = dir(cwd);
    let mut hits = Vec::new();
    for (index, meta) in metas.iter().enumerate() {
        let mut seen = vec![false; terms.len()];
        let head = format!("{} {}", meta.title, meta.summary).to_lowercase();
        mark(&head, &terms, &mut seen);
        let mut snippet = String::new();
        if seen.iter().any(|seen| !seen) && !meta.archived {
            let path = log_path(&dir, &meta.id);
            if let Ok(file) = File::open(&path) {
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(text) = message["content"].as_str() else {
                        continue;
                    };
                    let lower = text.to_lowercase();
                    let marked = terms.iter().any(|term| lower.contains(term));
                    if marked {
                        mark(&lower, &terms, &mut seen);
                        if snippet.is_empty() {
                            snippet = snip(text, &terms);
                        }
                    }
                    if seen.iter().all(|seen| *seen) {
                        break;
                    }
                }
            }
        }
        if seen.iter().all(|seen| *seen) {
            if snippet.is_empty() {
                snippet = snip(&meta.summary, &terms);
            }
            hits.push(Hit { index, snippet });
        }
    }
    hits
}

fn mark(text: &str, terms: &[String], seen: &mut [bool]) {
    for (at, term) in terms.iter().enumerate() {
        if !seen[at] && text.contains(term) {
            seen[at] = true;
        }
    }
}

/// The first line that contains a query term, else the first useful line.
/// Whitespace is collapsed so the row stays one line.
fn snip(text: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = lines
        .iter()
        .copied()
        .find(|line| terms.iter().any(|term| line.to_lowercase().contains(term)))
        .or_else(|| lines.first().copied())
        .unwrap_or("");
    collapse(line, 100)
}

/// One line, whitespace collapsed, capped.
pub fn collapse(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// A title and summary that need no model: the first request, then the first
/// answer. Used for an interrupted session and when naming fails.
pub fn fallback(messages: &[Value]) -> (String, String) {
    let user = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("user"))
        .filter_map(|m| m["content"].as_str())
        .find(|text| !text.starts_with("explore mode:") && !text.starts_with('$'))
        .map(|text| collapse(text, 240))
        .unwrap_or_default();
    if user.is_empty() {
        return (String::new(), String::new());
    }
    let assistant = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("assistant"))
        .filter_map(|m| m["content"].as_str())
        .find(|text| !text.trim().is_empty())
        .map(|text| collapse(text, 240))
        .unwrap_or_default();
    let mut title: String = user.chars().take(60).collect();
    if user.chars().count() > 60 {
        title.push('…');
    }
    let summary = if assistant.is_empty() {
        user
    } else {
        format!("{user} — {assistant}")
    };
    let summary: String = summary.chars().take(240).collect();
    (title, summary)
}

/// `YYYY-MM-DD HH:MM`, UTC. Date math stays here: a time crate would be a
/// dependency for one line of output.
pub fn stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rest / 3600,
        (rest % 3600) / 60
    )
}

/// Days since the epoch to a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_utc() {
        assert_eq!(stamp(0), "1970-01-01 00:00");
        assert_eq!(stamp(1_700_000_000), "2023-11-14 22:13");
    }

    #[test]
    fn meta_and_log_roundtrip() {
        let dir = std::env::temp_dir().join(format!("axe-test-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut meta = Meta::new("abc123", "prov/model");
        meta.title = "Title".into();
        meta.save(&dir).unwrap();
        let mut log = Log::open(&dir, "abc123").unwrap();
        log.append(&json!({ "role": "user", "content": "hello" }))
            .unwrap();
        log.append(&json!({ "role": "assistant", "content": "hi" }))
            .unwrap();
        assert_eq!(Meta::load(&dir, "abc123").unwrap().title, "Title");
        let mut meta = Meta::load(&dir, "abc123").unwrap();
        let mut warnings = Vec::new();
        let messages = load_messages(&dir, &mut meta, &mut warnings).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_uses_first_request_and_answer() {
        let messages = vec![
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "fix the parser" }),
            json!({ "role": "assistant", "content": "Done." }),
        ];
        let (title, summary) = fallback(&messages);
        assert_eq!(title, "fix the parser");
        assert!(summary.contains("Done."));
        assert_eq!(collapse("  a\n b  ", 20), "a b");
    }

    #[test]
    fn gzip_roundtrip() {
        if !have_gzip() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("axe-test-gzip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = Meta::new("abc123", "p/m");
        let plain = log_path(&dir, "abc123");
        let gz = gz_path(&dir, "abc123");
        std::fs::write(&plain, "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
        compress(&dir, &meta, &plain, &gz).unwrap();
        assert!(gz.exists());
        assert!(!plain.exists());
        unpack(&dir, &mut meta, &gz, &plain).unwrap();
        assert!(plain.exists());
        assert!(!gz.exists());
        assert!(std::fs::read_to_string(&plain).unwrap().contains("hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_body_is_reported() {
        let dir = std::env::temp_dir().join(format!("axe-test-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = Meta::new("abc123", "p/m");
        meta.save(&dir).unwrap();
        std::fs::write(
            log_path(&dir, "abc123"),
            "{\"role\":\"user\",\"content\":\"hi\"}\nnot json\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let body = load_messages(&dir, &mut meta, &mut warnings).unwrap();
        assert_eq!(body.len(), 1);
        assert!(
            warnings.iter().any(|w| w.contains("damaged")),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_marks_and_snips_the_matching_line() {
        let terms = vec!["needle".to_string()];
        let mut seen = vec![false];
        mark("hay needle", &terms, &mut seen);
        assert!(seen[0]);
        let text = "first line\n  the needle is here  \nlast";
        assert_eq!(snip(text, &terms), "the needle is here");
        let mut seen = vec![false];
        mark("none", &terms, &mut seen);
        assert!(!seen[0]);
    }
}
