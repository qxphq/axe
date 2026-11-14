//! The two requests that are not a turn: the summary `/compact` writes, and the
//! title and summary a new session is named with. Also what each is made of —
//! the transcript a summary sees, the tail kept verbatim beside it, and the
//! excerpt a title is written from. `main` decides when they run and what the
//! session does with the answer; this module decides what they send.

use serde_json::{json, Value};

use crate::api;
use crate::sessions;
use crate::text::head_tail;

/// The first request and the latest answer, short enough to name from. Naming
/// waits for an assistant message: a session that only ran `!cmd` gets the
/// fallback.
pub(crate) fn naming_excerpt(messages: &[Value]) -> Option<(String, String)> {
    if !messages
        .iter()
        .any(|message| message["role"].as_str() == Some("assistant"))
    {
        return None;
    }
    let user = messages
        .iter()
        .filter(|message| message["role"].as_str() == Some("user"))
        .filter_map(|message| message["content"].as_str())
        .find(|text| sessions::is_request(text))?;
    let assistant = messages
        .iter()
        .filter(|message| message["role"].as_str() == Some("assistant"))
        .filter_map(|message| message["content"].as_str())
        .rfind(|text| !text.trim().is_empty())
        .unwrap_or("");
    Some((
        sessions::collapse(user, 1500),
        sessions::collapse(assistant, 1500),
    ))
}

/// `/compact`'s instruction: the reply is the summary and nothing else.
pub(crate) const COMPACT_PROMPT: &str =
    "You summarize a coding session so it can continue in a smaller \
context. Reply with the summary only, no preamble. Keep every path, command, identifier, error \
message and decision exact. Sections: task, what has been done, files changed, commands run, \
decisions and constraints, open questions, next step. Under 800 words.";

/// What a reply that carried no text did instead, for the one place axe asks
/// for text it cannot show: `/compact`. The finish reason is the difference
/// between an answer cut off and a model that said nothing.
pub(crate) fn no_text(reply: &api::Reply) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(reason) = &reply.finish_reason {
        parts.push(format!("finish reason: {reason}"));
    }
    if let Some(usage) = &reply.usage {
        parts.push(format!("{} completion tokens", usage.completion));
    }
    if !reply.reasoning.trim().is_empty() {
        parts.push(format!(
            "{} chars of reasoning",
            reply.reasoning.chars().count()
        ));
    }
    if parts.is_empty() {
        "the provider sent no text".to_string()
    } else {
        parts.join(", ")
    }
}

/// Where the verbatim tail begins: pi's cut point. Walk back from the newest
/// message while the tail is under `keep` tokens and start keeping at the
/// oldest user or assistant message inside it — never at a tool result, which
/// would start the tail with an answer to a call it does not carry. A history
/// that fits inside the tail is summarized whole: a summary that replaces
/// nothing is not a compaction.
pub(crate) fn tail_start(messages: &[Value], keep: u64) -> usize {
    let mut tokens = 0u64;
    let mut start = 2.min(messages.len());
    for at in (start..messages.len()).rev() {
        tokens += crate::store::weight(&messages[at]) / 4;
        let role = messages[at]["role"].as_str().unwrap_or("");
        if role == "user" || role == "assistant" {
            start = at;
            if tokens >= keep {
                return start;
            }
        }
    }
    // The whole history is inside the tail: nothing would be replaced, so the
    // summary covers everything and there is no tail.
    messages.len()
}

pub(crate) fn compaction_prompt(messages: &[Value], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages.iter().skip(2) {
        let role = message["role"].as_str().unwrap_or("");
        if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
            let (head, tail) = if role == "tool" {
                (800, 200)
            } else {
                (2000, 500)
            };
            lines.push(format!("{role}: {}", head_tail(text, head, tail)));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let name = call["function"]["name"].as_str().unwrap_or("");
                let args = call["function"]["arguments"].as_str().unwrap_or("");
                lines.push(format!(
                    "{role}: calls {name} {}",
                    head_tail(args, 300, 100)
                ));
            }
        }
    }
    let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut dropped = 0;
    while total > budget && lines.len() > 1 {
        total -= lines[0].len() + 1;
        lines.remove(0);
        dropped += 1;
    }
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!("[{dropped} earlier messages omitted]\n"));
    }
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The two lines the naming prompt asks for. Anything else falls back.
pub(crate) fn parse_naming(text: &str) -> Option<(String, String)> {
    let mut title = None;
    let mut summary = None;
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim();
        if value.is_empty() {
            continue;
        }
        if key.trim().eq_ignore_ascii_case("title") {
            title = Some(value.to_string());
        } else if key.trim().eq_ignore_ascii_case("summary") {
            summary = Some(value.to_string());
        }
    }
    title.map(|title| (title, summary.unwrap_or_default()))
}

/// The naming request's two messages: the prompt, then what the session said.
/// One request for a title and a summary, because a second one would be a
/// second round for the same answer.
pub(crate) fn naming_messages(user: &str, assistant: &str) -> Vec<Value> {
    vec![
        json!({
            "role": "system",
            "content": "You name conversations. Reply with exactly two lines:\n\
                        TITLE: a descriptive title of three to six words\n\
                        SUMMARY: one sentence describing the task, under 25 words\n\
                        No quotes, no markdown, no extra lines."
        }),
        json!({
            "role": "user",
            "content": format!("User: {user}\nAssistant: {assistant}"),
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_parses_two_lines() {
        assert_eq!(
            parse_naming("TITLE: Fix parser\nSUMMARY: The parser was fixed."),
            Some(("Fix parser".into(), "The parser was fixed.".into()))
        );
        assert_eq!(parse_naming("nothing useful"), None);
    }

    #[test]
    fn compaction_prompt_keeps_the_newest_and_counts_what_it_dropped() {
        let messages = vec![
            json!({ "role": "system", "content": "the prompt" }),
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "first" }),
            json!({ "role": "assistant", "content": "second" }),
            json!({ "role": "user", "content": "third" }),
        ];
        // Wide enough: everything the session said, and neither the system
        // prompt nor the mode note.
        let all = compaction_prompt(&messages, 10_000);
        assert!(all.contains("user: first"));
        assert!(all.contains("assistant: second"));
        assert!(all.contains("user: third"));
        assert!(!all.contains("the prompt"));
        assert!(!all.contains("explore mode"));
        // Tight enough for one message: the oldest goes first, the newest stays.
        let one = compaction_prompt(&messages, 10);
        assert!(one.contains("third"));
        assert!(!one.contains("first"));
        assert!(one.starts_with("["));
        assert!(one.contains("earlier messages omitted"));
        // Nothing to say is not a panic.
        assert_eq!(compaction_prompt(&messages[..2], 10), "");
    }

    #[test]
    fn a_tool_result_is_cut_before_it_is_summarized() {
        let long = "x".repeat(5000);
        let messages = vec![
            json!({ "role": "system", "content": "s" }),
            json!({ "role": "user", "content": "u" }),
            json!({ "role": "tool", "content": long }),
            json!({ "role": "user", "content": "newest" }),
        ];
        let prompt = compaction_prompt(&messages, 10_000);
        assert!(prompt.contains("…[cut]…"));
        assert!(prompt.len() < 1500);
        assert!(prompt.contains("newest"));
        // Short text is left alone.
        assert_eq!(head_tail("abc", 2, 2), "abc");
    }

    #[test]
    fn naming_excerpt_skips_mode_and_shell_lines() {
        let messages = vec![
            json!({ "role": "user", "content": "explore mode: on" }),
            json!({ "role": "user", "content": "$ !ls\nfile" }),
            json!({ "role": "user", "content": "fix it" }),
            json!({ "role": "assistant", "content": "fixed" }),
        ];
        assert_eq!(
            naming_excerpt(&messages),
            Some(("fix it".into(), "fixed".into()))
        );
    }

    #[test]
    fn a_reply_without_text_names_what_it_did_instead() {
        let reply = api::Reply {
            text: String::new(),
            reasoning: "thought".into(),
            reasoning_field: Some("reasoning_content"),
            calls: Vec::new(),
            usage: Some(api::Usage {
                prompt: 900,
                completion: 2000,
                cached: 0,
            }),
            finish_reason: Some("length".into()),
            complete: true,
        };
        assert_eq!(
            no_text(&reply),
            "finish reason: length, 2000 completion tokens, 7 chars of reasoning"
        );

        let bare = api::Reply {
            reasoning: String::new(),
            usage: None,
            finish_reason: None,
            ..reply
        };
        assert_eq!(no_text(&bare), "the provider sent no text");
    }
}

#[test]
fn a_naming_reply_is_read_loosely_or_not_at_all() {
    assert_eq!(
        parse_naming("title: T\nsummary: S"),
        Some(("T".into(), "S".into()))
    );
    assert_eq!(
        parse_naming("TITLE: \"quoted\"\nSUMMARY: \"also quoted\""),
        Some(("quoted".into(), "also quoted".into()))
    );
    // A title with no summary is still a title.
    assert_eq!(
        parse_naming("TITLE: only"),
        Some(("only".into(), String::new()))
    );
    // No title, no name.
    assert_eq!(parse_naming("SUMMARY: orphan"), None);
    assert_eq!(parse_naming(""), None);
    assert_eq!(parse_naming("TITLE:\nSUMMARY: x"), None);
}
