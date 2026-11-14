//! Sessions: what one working directory remembers across runs.
//!
//! One directory per project holds a meta file and an append-only log for each
//! session. The meta is rewritten as the title and count change; the log is
//! never rewritten, so a crash can only leave one unfinished line, which the
//! loader skips. Old
//! bodies are gzipped in place by the host's `gzip`: the meta stays readable,
//! so listings and search keep working without unpacking anything, and a host
//! without gzip leaves the bodies plain.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::state;

// ---- a session on disk ----

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
    pub fn new(id: &str) -> Meta {
        let now = now();
        Meta {
            id: id.to_string(),
            created: now,
            updated: now,
            title: String::new(),
            summary: String::new(),
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

// ---- listing and loading ----

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
        let mut file = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        // A body created before the mode was set stays readable by other
        // local users until it is tightened here.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        // A kill mid-append leaves the last line unfinished. The next message
        // starts a line of its own: appended to the fragment, both are lost
        // when the body is next read.
        let len = file
            .metadata()
            .map_err(|e| format!("read {}: {e}", path.display()))?
            .len();
        if len > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))
                .and_then(|_| file.read_exact(&mut last))
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            if last[0] != b'\n' {
                file.write_all(b"\n")
                    .map_err(|e| format!("append session: {e}"))?;
            }
        }
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

// ---- compaction ----

/// The marker a compacted body carries. The message under it is the summary the
/// session continues from, and the count says how many messages it replaced.
/// The provider never sees either: the loader strips the key.
pub const COMPACT: &str = "axe_compact";

/// What the summary message says before the summary. The session's own voice,
/// not the user's: the places that look for what the user asked skip it, and a
/// resume starts from it without echoing it as a user line.
pub const COMPACT_NOTE: &str = "Earlier in this session, before it was compacted:";

/// The summary as the body records it.
pub fn compacted(content: &str, replaced: u64) -> Value {
    let mut message = json!({ "role": "user", "content": content });
    message[COMPACT] = json!({ "replaced": replaced });
    message
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
    let mut reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut marker = None;
    let mut damaged = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                warnings.push(format!("read {}: {e}", plain.display()));
                break;
            }
        }
        // A kill mid-append leaves a line with no message in it. It is
        // skipped, not the end: the session resumed after it, so everything
        // appended since is still this session's.
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            damaged = true;
            continue;
        };
        match message["role"].as_str() {
            Some("user" | "assistant" | "tool") => {
                if message.get(COMPACT).is_some() {
                    marker = Some(messages.len());
                }
                messages.push(message);
            }
            _ => continue,
        }
    }
    if damaged {
        warnings.push(format!("skipping a damaged line in {}", plain.display()));
    }
    // A compacted body continues from its last summary. What came before stays
    // on disk — the body is append-only — and out of the request, so replay
    // matches what the live session sent.
    if let Some(at) = marker {
        messages.drain(..at);
        for message in &mut messages {
            if let Some(object) = message.as_object_mut() {
                object.remove(COMPACT);
            }
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

// ---- archiving ----

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
    for listed in old {
        let Ok(mut meta) = Meta::load(&dir, &listed.id) else {
            continue;
        };
        // A session another instance touched since the listing is live: a
        // resume bumps `updated`, an append the count. Its body is not ours
        // to move.
        if meta.archived || !unchanged(&listed, &meta) {
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
    let before = fingerprint(plain)?;
    let tmp = dir.join(format!("{}.jsonl.gz.tmp", meta.id));
    gzip(&[OsStr::new("-nc"), plain.as_os_str()], &tmp)?;
    // A running axe appended while we compressed: the copy is stale, and
    // removing the body would take that message with it. The body stays plain
    // and the next start tries again.
    if fingerprint(plain)? != before {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "{} changed while it was compressed",
            plain.display()
        ));
    }
    fs::rename(&tmp, gz).map_err(|e| format!("write {}: {e}", gz.display()))?;
    fs::remove_file(plain).map_err(|e| format!("remove {}: {e}", plain.display()))?;
    Ok(())
}

/// Whether the sidecar a listing produced still describes the same session.
fn unchanged(listed: &Meta, fresh: &Meta) -> bool {
    listed.updated == fresh.updated && listed.messages == fresh.messages
}

/// What a body was before it was compressed: its size and its change time.
fn fingerprint(path: &Path) -> Result<(u64, Option<SystemTime>), String> {
    let meta = fs::metadata(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok((meta.len(), meta.modified().ok()))
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

// ---- search ----

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

// ---- text ----

/// One line, whitespace collapsed, capped.
pub fn collapse(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Whether a `user` line is the user's own request. The harness says things in
/// that role too — the mode note, the sandbox's caveat, a shell echo, a
/// compaction summary — and a title, a summary or a naming excerpt built from
/// one of them names the session after the machinery. One rule, because the
/// three places that pick "the user's words" must agree on what those are.
pub fn is_request(text: &str) -> bool {
    !(text.starts_with("explore mode:")
        || text.starts_with("sandbox:")
        || text.starts_with("bash is unavailable")
        || text.starts_with('$')
        || text.starts_with(COMPACT_NOTE))
}

/// A title and summary that need no model: the first request, then the first
/// answer. Used for an interrupted session and when naming fails.
pub fn fallback(messages: &[Value]) -> (String, String) {
    let user = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("user"))
        .filter_map(|m| m["content"].as_str())
        .find(|text| is_request(text))
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
        let mut meta = Meta::new("abc123");
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
    fn a_compacted_body_starts_at_its_last_summary() {
        let dir = std::env::temp_dir().join(format!("axe-test-compact-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let meta = Meta::new("abc123");
        meta.save(&dir).unwrap();
        let mut log = Log::open(&dir, "abc123").unwrap();
        log.append(&json!({ "role": "user", "content": "one" }))
            .unwrap();
        log.append(&compacted("first summary", 1)).unwrap();
        log.append(&json!({ "role": "user", "content": "two" }))
            .unwrap();
        log.append(&compacted("second summary", 2)).unwrap();
        log.append(&json!({ "role": "assistant", "content": "three" }))
            .unwrap();
        let mut meta = Meta::load(&dir, "abc123").unwrap();
        let mut warnings = Vec::new();
        let messages = load_messages(&dir, &mut meta, &mut warnings).unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0]["content"].as_str(), Some("second summary"));
        assert!(messages[0].get(COMPACT).is_none(), "the marker is stripped");
        assert_eq!(messages[1]["content"].as_str(), Some("three"));
        assert_eq!(meta.messages, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every line the harness speaks in the user's role is skipped, the
    /// sandbox's caveat included: on a kernel without Landlock that note is the
    /// first user message, and a title built from it names the session after
    /// the machinery.
    #[test]
    fn only_the_users_own_lines_are_requests() {
        for note in [
            "explore mode: on — write and edit are unavailable",
            "explore mode: off — write, edit and a full bash are available",
            "sandbox: Landlock is unavailable, so explore mode's bash is unconfined",
            "sandbox: this kernel's Landlock (ABI 2) cannot deny truncation",
            "bash is unavailable: /usr/bin/sandbox-exec is missing",
            "$ ls -la",
            "Earlier in this session, before it was compacted:\n\nsummary",
        ] {
            assert!(!is_request(note), "{note:?} is the harness's");
        }
        assert!(is_request("fix the parser"));
        assert!(is_request("sandbox the parser, please"));
        // The fallback title is the first real request, whatever came first.
        let history = vec![
            json!({ "role": "system", "content": "prompt" }),
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "sandbox: Landlock is unavailable" }),
            json!({ "role": "user", "content": "make it green" }),
            json!({ "role": "assistant", "content": "done" }),
        ];
        let (title, summary) = fallback(&history);
        assert_eq!(title, "make it green");
        assert_eq!(summary, "make it green — done");
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
        // A compacted session opens with the summary, which is the session's
        // own voice: the request it names is the first one after it.
        let compacted = vec![
            json!({ "role": "user", "content": format!("{COMPACT_NOTE}\n\nthe summary") }),
            json!({ "role": "user", "content": "next thing" }),
        ];
        let (title, summary) = fallback(&compacted);
        assert_eq!(title, "next thing");
        assert!(!summary.contains(COMPACT_NOTE), "{summary}");
    }

    #[test]
    fn gzip_roundtrip() {
        if !have_gzip() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("axe-test-gzip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = Meta::new("abc123");
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
        let mut meta = Meta::new("abc123");
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
    fn a_damaged_line_does_not_end_the_replay() {
        let dir = crate::state::testenv::dir("sessions-damaged-middle");
        let mut meta = Meta::new("abc123");
        meta.save(&dir).unwrap();
        let path = log_path(&dir, "abc123");
        // A kill mid-append: the last line is half a message.
        let one = json!({ "role": "user", "content": "one" }).to_string();
        let cut = r#"{"role":"assistant","content":"tw"#;
        std::fs::write(&path, format!("{one}\n{cut}")).unwrap();

        // The next session starts its line of its own, so the message it
        // writes is not swallowed by the fragment.
        let mut log = Log::open(&dir, "abc123").unwrap();
        log.append(&json!({ "role": "user", "content": "three" }))
            .unwrap();
        drop(log);

        let mut warnings = Vec::new();
        let body = load_messages(&dir, &mut meta, &mut warnings).unwrap();
        assert_eq!(body.len(), 2, "{body:?}");
        assert_eq!(body[1]["content"], "three");
        assert!(
            warnings.iter().any(|w| w.contains("damaged")),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_body_that_grew_is_not_the_one_that_was_compressed() {
        let dir = crate::state::testenv::dir("sessions-fingerprint");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("body.jsonl");
        std::fs::write(&path, "one\n").unwrap();
        let before = fingerprint(&path).unwrap();
        std::fs::write(&path, "one\ntwo\n").unwrap();
        assert_ne!(fingerprint(&path).unwrap(), before);

        // The sidecar comparison: a resume bumps `updated`, an append the count.
        let listed = Meta::new("abc123");
        assert!(unchanged(&listed, &listed.clone()));
        let mut resumed = listed.clone();
        resumed.updated += 1;
        assert!(!unchanged(&listed, &resumed));
        let mut appended = listed.clone();
        appended.messages += 1;
        assert!(!unchanged(&listed, &appended));
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

    use crate::state::testenv;

    /// One session with a title and a body of user messages, stamped so the
    /// listing order is the test's, not the clock's.
    fn session(dir: &Path, id: &str, title: &str, lines: &[&str], updated: u64) {
        let mut meta = Meta::new(id);
        meta.title = title.to_string();
        meta.updated = updated;
        meta.save(dir).unwrap();
        let mut log = Log::open(dir, id).unwrap();
        for line in lines {
            log.append(&json!({ "role": "user", "content": line }))
                .unwrap();
        }
    }

    #[test]
    fn list_orders_by_activity_and_skips_junk() {
        testenv::home("sessions-list", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = dir(&cwd);
            session(&dir, "aa11", "older", &[], 10);
            session(&dir, "bb22", "newer", &[], 20);
            // A body with no sidecar, an id that is not hex, and a sidecar that
            // does not parse are all skipped, not fatal.
            std::fs::write(dir.join("cc33.jsonl"), "").unwrap();
            std::fs::write(dir.join("not-hex.json"), "{}").unwrap();
            std::fs::write(dir.join("dd44.json"), "{").unwrap();
            let ids: Vec<String> = list(&cwd).into_iter().map(|meta| meta.id).collect();
            assert_eq!(ids, vec!["bb22".to_string(), "aa11".to_string()]);
            // A directory that does not exist lists nothing.
            assert!(list(&home.join("nowhere")).is_empty());
        });
    }

    #[test]
    fn a_sidecar_is_validated_before_it_is_believed() {
        let dir = testenv::dir("sessions-meta");
        assert!(Meta::load(&dir, "not-a-session").is_err());
        assert!(Meta::load(&dir, "abc123").is_err(), "nothing to read");
        std::fs::write(dir.join("abc123.json"), r#"{"v": 99}"#).unwrap();
        let err = Meta::load(&dir, "abc123").err().unwrap();
        assert!(err.contains("newer format"), "{err}");
        std::fs::write(dir.join("abc123.json"), r#"{"v": 1}"#).unwrap();
        let meta = Meta::load(&dir, "abc123").unwrap();
        assert_eq!(meta.title, "");
        assert_eq!(meta.messages, 0);
        assert!(valid_id("abc123"));
        assert!(valid_id("ABC123"), "hex digits are digits either case");
        assert!(!valid_id("abc xyz"));
        assert!(!valid_id(""));
        assert!(!valid_id(&"a".repeat(65)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_reads_titles_summaries_and_bodies() {
        testenv::home("sessions-search", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = dir(&cwd);
            let mut meta = Meta::new("aa11");
            meta.title = "Parse the sessions".into();
            meta.summary = "a parse-heavy session".into();
            meta.updated = 20;
            meta.save(&dir).unwrap();
            let mut log = Log::open(&dir, "aa11").unwrap();
            log.append(&json!({ "role": "user", "content": "the needle is here" }))
                .unwrap();
            session(&dir, "bb22", "Nothing to see", &["unrelated"], 10);
            let metas = list(&cwd);
            assert_eq!(metas[0].id, "aa11");

            // A term only the title holds.
            let hits = search(&cwd, &metas, "parse");
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].index, 0);
            assert!(hits[0].snippet.contains("parse"), "{}", hits[0].snippet);

            // A term only the body holds: the snippet is the line it is on.
            let hits = search(&cwd, &metas, "needle");
            assert_eq!(hits[0].snippet, "the needle is here");

            // Every term must match, and they may match different places.
            assert_eq!(search(&cwd, &metas, "parse needle").len(), 1);
            assert!(search(&cwd, &metas, "parse unrelated").is_empty());

            // No terms: every session, and no snippet to show.
            let all = search(&cwd, &metas, "   ");
            assert_eq!(all.len(), 2);
            assert!(all.iter().all(|hit| hit.snippet.is_empty()));

            // An archived body is not read; title and summary still answer.
            let mut archived = metas.clone();
            archived[0].archived = true;
            assert!(search(&cwd, &archived, "needle").is_empty());
            assert_eq!(search(&cwd, &archived, "parse").len(), 1);
        });
    }

    #[test]
    fn archive_old_compresses_the_bodies_past_the_keep_count() {
        if !have_gzip() {
            return;
        }
        testenv::home("sessions-archive", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = dir(&cwd);
            session(&dir, "aa11", "oldest", &["one"], 10);
            session(&dir, "bb22", "middle", &["two"], 20);
            session(&dir, "cc33", "newest", &["three"], 30);
            archive_old(&cwd, 1);

            let metas = list(&cwd);
            let archived: Vec<&str> = metas
                .iter()
                .filter(|meta| meta.archived)
                .map(|meta| meta.id.as_str())
                .collect();
            assert_eq!(archived, vec!["bb22", "aa11"]);
            assert!(gz_path(&dir, "aa11").is_file());
            assert!(!log_path(&dir, "aa11").exists());
            assert!(log_path(&dir, "cc33").is_file(), "the kept one stays plain");

            // Reading an archived session unpacks it and makes it live again.
            let mut meta = metas.iter().find(|meta| meta.id == "aa11").unwrap().clone();
            let mut warnings = Vec::new();
            let body = load_messages(&dir, &mut meta, &mut warnings).unwrap();
            assert_eq!(body.len(), 1);
            assert!(warnings.is_empty(), "{warnings:?}");
            assert!(!meta.archived);
            assert!(log_path(&dir, "aa11").is_file());
            assert!(!gz_path(&dir, "aa11").exists());
            assert!(!Meta::load(&dir, "aa11").unwrap().archived);
        });
    }

    #[test]
    fn a_body_that_is_missing_is_reported() {
        let dir = testenv::dir("sessions-nobody");
        let mut meta = Meta::new("abc123");
        let mut warnings = Vec::new();
        let err = load_messages(&dir, &mut meta, &mut warnings).err().unwrap();
        assert!(err.contains("has no body"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_handles_what_is_missing() {
        assert_eq!(fallback(&[]), (String::new(), String::new()));
        let (title, summary) = fallback(&[json!({ "role": "user", "content": "only a request" })]);
        assert_eq!(title, "only a request");
        assert_eq!(summary, "only a request");
        // A long request is cut for the title and again for the summary, and
        // the assistant's own words join it.
        let long = "word ".repeat(20);
        let (title, summary) = fallback(&[
            json!({ "role": "user", "content": long }),
            json!({ "role": "assistant", "content": "   " }),
            json!({ "role": "assistant", "content": "the answer" }),
        ]);
        assert!(title.ends_with('…'), "{title}");
        assert!(title.chars().count() <= 61, "{title}");
        assert!(summary.contains("the answer"), "{summary}");
        assert!(summary.chars().count() <= 240, "{summary}");
        assert_eq!(collapse(&"x".repeat(20), 3), "xxx…");
        assert_eq!(collapse("x", 3), "x");
    }

    #[test]
    fn dates_survive_leap_days_and_the_epoch() {
        assert_eq!(civil(0), (1970, 1, 1));
        // 2000-02-29, a leap day in a leap year.
        assert_eq!(civil(11_016), (2000, 2, 29));
        // 1970-01-01 is day zero; the day before it is 1969.
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(stamp(11_016 * 86_400 + 3_600), "2000-02-29 01:00");
        assert_eq!(stamp(1_709_164_800), "2024-02-29 00:00");
    }

    #[test]
    fn a_failed_gzip_leaves_no_partial_file() {
        let dir = testenv::dir("sessions-gzip-fail");
        let out = dir.join("out.gz");
        let missing = dir.join("no-such-file");
        let err = gzip(&[OsStr::new("-nc"), missing.as_os_str()], &out)
            .err()
            .unwrap();
        assert!(err.starts_with("gzip"), "{err}");
        assert!(!out.exists(), "a failed run removes what it started");

        // The output cannot be created at all: the directory is not there.
        let err = gzip(
            &[OsStr::new("-nc"), missing.as_os_str()],
            &dir.join("no/dir.gz"),
        )
        .err()
        .unwrap();
        assert!(err.starts_with("create"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_compressed_body_left_beside_its_plain_copy_is_finished_off() {
        if !have_gzip() {
            return;
        }
        let dir = testenv::dir("sessions-crash");
        let meta = Meta::new("abc123");
        let plain = log_path(&dir, "abc123");
        let gz = gz_path(&dir, "abc123");
        std::fs::write(&plain, "plain\n").unwrap();
        gzip(&[OsStr::new("-nc"), plain.as_os_str()], &gz).unwrap();
        // A crash after the rename but before the plain copy went away.
        compress(&dir, &meta, &plain, &gz).unwrap();
        assert!(!plain.exists(), "the plain copy is removed");
        assert!(gz.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_session_already_compressed_only_gets_its_marker() {
        if !have_gzip() {
            return;
        }
        testenv::home("sessions-marker", |home| {
            let cwd = home.join("work");
            std::fs::create_dir_all(&cwd).unwrap();
            let dir = dir(&cwd);
            // A body that is compressed but whose sidecar never learned it.
            let mut meta = Meta::new("abc123");
            meta.updated = 1;
            meta.save(&dir).unwrap();
            let plain = log_path(&dir, "abc123");
            std::fs::write(&plain, "{\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
            gzip(
                &[OsStr::new("-nc"), plain.as_os_str()],
                &gz_path(&dir, "abc123"),
            )
            .unwrap();
            std::fs::remove_file(&plain).unwrap();

            archive_old(&cwd, 0);
            assert!(Meta::load(&dir, "abc123").unwrap().archived);
            // Nothing left to do the second time.
            archive_old(&cwd, 0);
            assert!(Meta::load(&dir, "abc123").unwrap().archived);
            // And there is nothing to archive when the keep count covers it.
            archive_old(&cwd, 5);
        });
    }

    #[test]
    fn a_body_count_is_corrected_when_the_file_disagrees() {
        let dir = testenv::dir("sessions-count");
        let mut meta = Meta::new("abc123");
        meta.messages = 9;
        meta.save(&dir).unwrap();
        let mut log = Log::open(&dir, "abc123").unwrap();
        log.append(&json!({ "role": "user", "content": "one" }))
            .unwrap();
        // The sidecar said nine; the body says one, and the body wins.
        let mut warnings = Vec::new();
        let body = load_messages(&dir, &mut meta, &mut warnings).unwrap();
        assert_eq!(body.len(), 1);
        assert_eq!(meta.messages, 1);
        assert_eq!(Meta::load(&dir, "abc123").unwrap().messages, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
