//! The session on disk and the live history that feeds it: one `push` appends to
//! both, so a message cannot reach the model without reaching the file, and a
//! resumed session replays the same order. The history's size in bytes is
//! counted from the same append, because the info row needs it and the request
//! is not serialized twice.

use serde_json::Value;

use crate::sessions;
use crate::turn;

/// What the summary message says before the summary. The session's own voice,
/// not the user's: `user_summary` and `naming_excerpt` skip it.
pub(crate) const COMPACT_NOTE: &str = "Earlier in this session, before it was compacted:";

/// What a turn appends to: the live history and, when this directory has one,
/// the session's log. One `push`, so a message cannot reach the model without
/// reaching the file, and a resumed session replays the same order.
pub(crate) struct Live<'a> {
    pub(crate) messages: &'a mut Vec<Value>,
    pub(crate) bytes: &'a mut u64,
    pub(crate) store: Option<&'a mut Store>,
}

impl turn::History for Live<'_> {
    fn push(&mut self, message: Value) {
        if let Some(store) = self.store.as_deref_mut() {
            store.record(&message);
        }
        *self.bytes += weight(&message);
        self.messages.push(message);
    }

    fn messages(&self) -> &[Value] {
        self.messages.as_slice()
    }
}

/// The session on disk. `meta` is loaded with the first message or on resume,
/// so an idle launch leaves no file behind.
pub(crate) struct Store {
    pub(crate) dir: std::path::PathBuf,
    id: String,
    log: Option<sessions::Log>,
    pub(crate) meta: Option<sessions::Meta>,
    pub(crate) naming_tried: bool,
    warned: bool,
    warning: Option<String>,
}

impl Store {
    pub(crate) fn fresh(dir: std::path::PathBuf, id: &str) -> Store {
        Store {
            dir,
            id: id.to_string(),
            log: None,
            meta: None,
            naming_tried: false,
            warned: false,
            warning: None,
        }
    }

    pub(crate) fn resumed(dir: std::path::PathBuf, meta: sessions::Meta) -> Result<Store, String> {
        let log = sessions::Log::open(&dir, &meta.id)?;
        Ok(Store {
            dir,
            id: meta.id.clone(),
            log: Some(log),
            meta: Some(meta),
            naming_tried: false,
            warned: false,
            warning: None,
        })
    }

    /// A write failure is reported once and the session goes on: a session log
    /// must never fail a turn.
    pub(crate) fn record(&mut self, message: &Value) {
        if let Err(e) = self.try_record(message) {
            if !self.warned {
                self.warned = true;
                self.warn(format!("session: {e}"));
            }
        }
    }

    fn warn(&mut self, text: String) {
        if self.warning.is_none() {
            self.warning = Some(text);
        }
    }

    pub(crate) fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    fn try_record(&mut self, message: &Value) -> Result<(), String> {
        if self.log.is_none() {
            self.log = Some(sessions::Log::open(&self.dir, &self.id)?);
        }
        if self.meta.is_none() {
            let mut meta = sessions::Meta::new(&self.id);
            if let Some(summary) = user_summary(message) {
                meta.summary = summary;
            }
            self.meta = Some(meta);
        }
        let meta = self.meta.as_mut().expect("just created");
        self.log.as_mut().expect("just opened").append(message)?;
        meta.messages += 1;
        meta.updated = sessions::now();
        if meta.summary.is_empty() {
            if let Some(summary) = user_summary(message) {
                meta.summary = summary;
            }
        }
        meta.save(&self.dir)
    }

    /// Record a compaction: the marked summary goes into the body, and the
    /// session's message count starts over at it. What came before is still in
    /// the file; it is no longer part of the session.
    pub(crate) fn compacted(&mut self, message: &Value) {
        self.record(message);
        if let Some(meta) = self.meta.as_mut() {
            meta.messages = 1;
            if let Err(e) = meta.save(&self.dir) {
                self.warn(format!("session meta: {e}"));
            }
        }
    }

    pub(crate) fn set_title(&mut self, title: &str, summary: &str) {
        let Some(meta) = self.meta.as_mut() else {
            return;
        };
        meta.title = sessions::collapse(title, 60);
        if !summary.trim().is_empty() {
            meta.summary = sessions::collapse(summary, 240);
        }
        if let Err(e) = meta.save(&self.dir) {
            self.warn(format!("session meta: {e}"));
        }
    }

    /// A title no model gave: the first request stands in. Called when the
    /// session ends without one.
    pub(crate) fn finalize(&mut self, messages: &[Value]) {
        let Some(meta) = self.meta.as_mut() else {
            return;
        };
        if meta.title.is_empty() {
            let (title, summary) = sessions::fallback(messages);
            if !title.is_empty() {
                meta.title = title;
                if meta.summary.is_empty() {
                    meta.summary = summary;
                }
            }
        }
        if let Err(e) = meta.save(&self.dir) {
            self.warn(format!("session meta: {e}"));
        }
    }
}

fn user_summary(message: &Value) -> Option<String> {
    if message["role"].as_str() != Some("user") {
        return None;
    }
    let text = message["content"].as_str()?;
    if text.starts_with("explore mode:") || text.starts_with('$') || text.starts_with(COMPACT_NOTE)
    {
        return None;
    }
    Some(sessions::collapse(text, 240))
}

/// How much of a history one message is, in the bytes the request carries. The
/// fill is estimated from this sum, four bytes to a token: the usual rule of
/// thumb for English, and off by tens of percent.
pub(crate) fn weight(message: &Value) -> u64 {
    message.to_string().len() as u64
}

/// The same sum over a whole history, for the points that replace it —
/// startup, resume, `/new`, `/compact` — rather than append to it.
pub(crate) fn history_bytes(messages: &[Value]) -> u64 {
    messages.iter().map(weight).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn store_records_and_finalizes() {
        let dir = std::env::temp_dir().join(format!("axe-test-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = Store::fresh(dir.clone(), "abc123");
        store.record(&json!({ "role": "user", "content": "fix it" }));
        assert!(store.take_warning().is_none());
        assert_eq!(store.meta.as_ref().unwrap().messages, 1);
        store.set_title("T", "S");
        assert_eq!(store.meta.as_ref().unwrap().title, "T");
        store.finalize(&[]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_summary_skips_the_notes_and_the_shell_lines() {
        // "explore mode:" and "$" lines are the harness's, not the user's.
        assert_eq!(
            user_summary(&json!({ "role": "user", "content": "do it" })),
            Some("do it".into())
        );
        assert!(user_summary(&json!({ "role": "user", "content": "$ ls" })).is_none());
        assert!(user_summary(&json!({ "role": "user", "content": "explore mode: on" })).is_none());
        assert!(user_summary(&json!({ "role": "assistant", "content": "hi" })).is_none());
        assert!(user_summary(&json!({ "role": "user", "content": "explore mode: on" })).is_none());
        assert_eq!(
            user_summary(
                &json!({ "role": "user", "content": "Earlier in this session, before it was compacted:\n\nsummary" })
            ),
            None,
            "a compaction summary is not the user's request"
        );
    }
}
