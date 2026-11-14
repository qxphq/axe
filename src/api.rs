//! The OpenAI-compatible chat API: model listing, the chat request, SSE
//! parsing, tool-call accumulation, and usage.

use std::time::Duration;

use serde_json::{json, Value};

use crate::http::{self, Cancel, Endpoint, Response};

/// The most parallel tool calls one round may name. A provider's `index` sizes
/// an array here, and it is the provider's number: past this the call is
/// dropped, not trusted.
const CALLS_MAX: u64 = 8;

/// One row of the provider's model list.
#[derive(Clone)]
pub struct ModelInfo {
    pub id: String,
    /// The context window when the provider reports one; the info row's fill
    /// has no denominator without it.
    pub context: Option<u64>,
}

/// A call a round named, with its arguments as the JSON text the provider
/// streamed: they are parsed only when the tool runs.
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub struct Reply {
    pub text: String,
    /// Thinking-mode chain of thought. Providers that enable tools require it
    /// back on every assistant message, so the caller keeps it in the history.
    pub reasoning: String,
    /// The field the reasoning streamed in (`reasoning_content` or
    /// `reasoning`): a reply goes back under the name it arrived with, so a
    /// provider that reads one is not handed the other.
    pub reasoning_field: Option<&'static str>,
    pub calls: Vec<ToolCall>,
    /// Tokens the provider counted for this round, when it reports them: the
    /// whole request, so it is how full the window was.
    pub usage: Option<Usage>,
    /// Why the stream ended: `stop`, `length`, `tool_calls`. A reply with no
    /// text can only be explained by this and the token count.
    pub finish_reason: Option<String>,
    /// Whether the stream ended the way the protocol says: `[DONE]`, or a
    /// finish reason on the choice. Without either, the connection stopped
    /// mid-answer, and what arrived is only part of one.
    pub complete: bool,
}

impl Reply {
    /// Why the reply is not an answer the model finished, if it is not: the
    /// provider cut it at its own limit, or the stream died before the
    /// protocol ended it. `None` is a whole answer.
    pub fn cut(&self) -> Option<&'static str> {
        if !self.complete {
            return Some("the stream ended before the reply did");
        }
        match self.finish_reason.as_deref() {
            Some("length") => Some("the reply hit the provider's length limit"),
            Some("content_filter") => Some("the provider stopped the reply (content filter)"),
            _ => None,
        }
    }
}

/// What the provider counted for one request: the whole prompt it read
/// (history included), the answer it wrote, and how much of that prompt was a
/// cache hit. Providers disagree on where a hit is reported — OpenAI and
/// OpenRouter under `prompt_tokens_details.cached_tokens`, DeepSeek as
/// `prompt_cache_hit_tokens`, Kimi as a top-level `cached_tokens` — so all
/// three are read and added as one number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
    pub cached: u64,
}

pub fn list_models(
    ep: &Endpoint,
    path: &str,
    api_key: Option<&str>,
    headers: &[(String, String)],
) -> Result<Vec<ModelInfo>, String> {
    // A listing is startup: bounded, so one dead provider cannot hold the
    // prompt off the screen for the five minutes a chat request is allowed.
    // Ten seconds is what an MCP handshake gets: every real answer — a cold
    // TLS handshake, a proxy roundtrip — is inside it, and a host that accepts
    // and never speaks is bounded by exactly this number.
    let mut res = http::request_with_timeout(
        ep,
        "GET",
        path,
        api_key,
        headers,
        None,
        None,
        None,
        Duration::from_secs(10),
    )?;
    let body = res.read_all().map_err(|e| format!("read models: {e}"))?;
    if res.status != 200 {
        return Err(format!("models: HTTP {}: {}", res.status, body.trim()));
    }
    parse_models(&body)
}

fn parse_models(body: &str) -> Result<Vec<ModelInfo>, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("models json: {e}"))?;
    let data = v["data"]
        .as_array()
        .ok_or_else(|| "models: missing \"data\" array".to_string())?;

    let mut models = Vec::new();
    for entry in data {
        if let Some(id) = entry["id"].as_str() {
            let context = entry["context_length"]
                .as_u64()
                .or_else(|| entry["context_window"].as_u64())
                // opencode reports the window under `limit`, with the output cap.
                .or_else(|| entry["limit"]["context"].as_u64());
            models.push(ModelInfo {
                id: id.to_string(),
                context,
            });
        }
    }
    Ok(models)
}

/// `extra` is merged into the request body last, so a thinking level can add
/// provider-specific fields without per-provider code paths.
#[allow(clippy::too_many_arguments)]
pub fn chat(
    ep: &Endpoint,
    api_key: Option<&str>,
    headers: &[(String, String)],
    model: &str,
    messages: &[Value],
    tools: &Value,
    extra: Option<&Value>,
    cancel: &Cancel,
) -> Result<Response, String> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "tools": tools,
        // Ask for the final chunk's token counts. Servers that do not know the
        // field ignore it, and then no `usage` arrives at all.
        "stream_options": { "include_usage": true },
    });
    if let (Some(Value::Object(extra)), Some(obj)) = (extra, body.as_object_mut()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    let body = body.to_string();
    http::request(
        ep,
        "POST",
        "/chat/completions",
        api_key,
        headers,
        Some(&body),
        Some(cancel),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn stream_chat<F: FnMut(&str), G: FnMut(&str)>(
    ep: &Endpoint,
    api_key: Option<&str>,
    headers: &[(String, String)],
    model: &str,
    messages: &[Value],
    tools: &Value,
    extra: Option<&Value>,
    cancel: &Cancel,
    mut on_text: F,
    mut on_reasoning: G,
) -> Result<Reply, String> {
    let mut res = chat(ep, api_key, headers, model, messages, tools, extra, cancel)?;
    if res.status != 200 {
        let body = res.read_all().unwrap_or_default();
        return Err(format!("HTTP {}: {}", res.status, body.trim()));
    }

    let mut assembler = Assembler::default();
    // The stream's own terminator. A close-delimited body ends with the
    // connection, so only `[DONE]` or a finish reason says the reply is whole.
    let mut done = false;
    loop {
        let line = match res.read_line() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => return Err(format!("read stream: {e}")),
        };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            done = true;
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        assembler.absorb(&chunk, &mut on_text, &mut on_reasoning)?;
    }
    let mut reply = assembler.finish();
    reply.complete = done || reply.finish_reason.is_some();
    Ok(reply)
}

#[derive(Default)]
struct Assembler {
    text: String,
    reasoning: String,
    reasoning_field: Option<&'static str>,
    partials: Vec<(String, String, String)>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
}

impl Assembler {
    fn absorb(
        &mut self,
        chunk: &Value,
        on_text: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        // Some compatible servers report an error inside a 200 SSE stream.
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            let message = error["message"]
                .as_str()
                .or_else(|| error.as_str())
                .unwrap_or("provider error");
            return Err(format!("provider: {message}"));
        }

        // The usage chunk comes last and carries no choices at all.
        if let Some(prompt) = chunk["usage"]["prompt_tokens"].as_u64() {
            let usage = &chunk["usage"];
            let cached = usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
                .or_else(|| usage["cached_tokens"].as_u64())
                .unwrap_or(0);
            self.usage = Some(Usage {
                prompt,
                completion: usage["completion_tokens"].as_u64().unwrap_or(0),
                cached,
            });
        }

        let choice = &chunk["choices"][0];
        // The chunk that ends a choice carries the reason and no delta. Some
        // servers send an empty string rather than omitting it.
        if let Some(reason) = choice["finish_reason"].as_str().filter(|r| !r.is_empty()) {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        // Providers disagree on the key; any non-empty delta means the model is
        // still reasoning and no answer has started.
        let reasoning = delta["reasoning_content"]
            .as_str()
            .map(|text| (text, "reasoning_content"))
            .or_else(|| delta["reasoning"].as_str().map(|text| (text, "reasoning")));
        if let Some((reasoning, field)) = reasoning.filter(|(text, _)| !text.is_empty()) {
            self.reasoning_field = Some(field);
            self.reasoning.push_str(reasoning);
            on_reasoning(reasoning);
        }
        if let Some(part) = delta["content"].as_str() {
            self.text.push_str(part);
            on_text(part);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                // The array below is sized from a number the provider chose,
                // so the number is capped: past it the call is dropped and the
                // round it named stays a request the provider will accept.
                let index = call["index"].as_u64().unwrap_or(0);
                if index >= CALLS_MAX {
                    continue;
                }
                let index = index as usize;
                while self.partials.len() <= index {
                    self.partials
                        .push((String::new(), String::new(), String::new()));
                }
                let partial = &mut self.partials[index];
                if let Some(id) = call["id"].as_str() {
                    if !id.is_empty() {
                        partial.0 = id.to_string();
                    }
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    if !name.is_empty() {
                        partial.1 = name.to_string();
                    }
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    partial.2.push_str(args);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Reply {
        let calls = self
            .partials
            .into_iter()
            .filter(|(_, name, _)| !name.is_empty())
            .map(|(id, name, arguments)| ToolCall {
                id,
                name,
                arguments,
            })
            .collect();
        Reply {
            text: self.text,
            reasoning: self.reasoning,
            reasoning_field: self.reasoning_field,
            calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
            // The stream told the reader; only `stream_chat` knows it.
            complete: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lists_read_each_context_key() {
        let models = parse_models(
            r#"{"data":[
                {"id":"a","context_length":100},
                {"id":"b","context_window":200},
                {"id":"c","limit":{"context":300}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].context, Some(100));
        assert_eq!(models[1].context, Some(200));
        assert_eq!(models[2].context, Some(300));
    }

    /// A cache hit is reported in three places by three providers; all three
    /// are read, and the reasoning's own field is remembered with it.
    #[test]
    fn a_usage_chunk_reports_its_cache_hit_wherever_it_lands() {
        for usage in [
            serde_json::json!({"prompt_tokens": 100, "completion_tokens": 5,
                               "prompt_tokens_details": {"cached_tokens": 60}}),
            serde_json::json!({"prompt_tokens": 100, "completion_tokens": 5,
                               "prompt_cache_hit_tokens": 60}),
            serde_json::json!({"prompt_tokens": 100, "completion_tokens": 5,
                               "cached_tokens": 60}),
            serde_json::json!({"prompt_tokens": 100, "completion_tokens": 5}),
        ] {
            let mut a = Assembler::default();
            a.absorb(
                &serde_json::json!({"usage": usage}),
                &mut |_| {},
                &mut |_| {},
            )
            .unwrap();
            let reply = a.finish();
            let counted = reply.usage.expect("a count");
            assert_eq!(counted.prompt, 100);
            assert_eq!(counted.completion, 5);
            let want = usage
                .get("prompt_tokens_details")
                .and_then(|details| details["cached_tokens"].as_u64())
                .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
                .or_else(|| usage["cached_tokens"].as_u64())
                .unwrap_or(0);
            assert_eq!(counted.cached, want, "{usage}");
        }
        // The field the reasoning streamed in is the field it goes back in.
        let mut a = Assembler::default();
        a.absorb(
            &serde_json::json!({"choices": [{"delta": {"reasoning": "thought"}}]}),
            &mut |_| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(a.finish().reasoning_field, Some("reasoning"));
    }

    #[test]
    fn assembler_collects_text_reasoning_calls_and_usage() {
        let mut a = Assembler::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut on_text = |part: &str| text.push_str(part);
        let mut on_reasoning = |part: &str| reasoning.push_str(part);
        a.absorb(
            &serde_json::json!({"choices":[{"delta":{"reasoning_content":"think"}}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        a.absorb(
            &serde_json::json!({"choices":[{"delta":{"content":"answer"}}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        a.absorb(
            &serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"path\":"}},
                {"index":0,"function":{"arguments":"\"x\"}"}}
            ]}}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        a.absorb(
            &serde_json::json!({"usage":{"prompt_tokens":7,"completion_tokens":3}}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        let reply = a.finish();
        assert_eq!(reply.text, "answer");
        assert_eq!(reply.reasoning, "think");
        assert_eq!(text, "answer");
        assert_eq!(reasoning, "think");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].name, "read");
        assert_eq!(reply.calls[0].arguments, "{\"path\":\"x\"}");
        assert_eq!(reply.usage.unwrap().prompt, 7);
        assert!(reply.finish_reason.is_none());
    }

    #[test]
    fn a_call_index_past_the_cap_is_dropped() {
        let mut a = Assembler::default();
        let mut on_text = |_: &str| {};
        let mut on_reasoning = |_: &str| {};
        // An index is an integer the provider chooses; it must not size the
        // assembler's array.
        for index in [1_000_000_000u64, u64::MAX] {
            a.absorb(
                &serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":index,"id":"far","function":{"name":"read","arguments":"{}"}}
                ]}}]}),
                &mut on_text,
                &mut on_reasoning,
            )
            .unwrap();
        }
        a.absorb(
            &serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"c1","function":{"name":"about","arguments":"{}"}}
            ]}}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        let reply = a.finish();
        assert_eq!(reply.calls.len(), 1, "only the index the cap allows");
        assert_eq!(reply.calls[0].id, "c1");
        assert_eq!(reply.calls[0].name, "about");
    }

    #[test]
    fn the_reason_a_choice_ended_is_kept() {
        let mut a = Assembler::default();
        let mut on_text = |_: &str| {};
        let mut on_reasoning = |_: &str| {};
        a.absorb(
            &serde_json::json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        assert_eq!(a.finish().finish_reason.as_deref(), Some("length"));

        // The empty string some servers send instead of omitting the field is
        // not a reason.
        let mut b = Assembler::default();
        b.absorb(
            &serde_json::json!({"choices":[{"delta":{"content":"x"},"finish_reason":""}]}),
            &mut on_text,
            &mut on_reasoning,
        )
        .unwrap();
        assert!(b.finish().finish_reason.is_none());
    }

    #[test]
    fn assembler_surfaces_provider_errors() {
        let mut a = Assembler::default();
        let mut on_text = |_: &str| {};
        let mut on_reasoning = |_: &str| {};
        let err = a
            .absorb(
                &serde_json::json!({"error":{"message":"bad key"}}),
                &mut on_text,
                &mut on_reasoning,
            )
            .unwrap_err();
        assert!(err.contains("bad key"));
    }

    #[test]
    fn a_model_list_reads_over_the_wire() {
        let server = crate::http::support::Server::new(vec![crate::http::support::json(
            r#"{"data":[{"id":"local/qwen3","context_window":8192}]}"#,
        )]);
        let models = list_models(&server.ep, "/models", Some("k"), &[]).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "local/qwen3");
        assert_eq!(models[0].context, Some(8192));
        assert!(server.text().starts_with("GET /models "));
    }

    #[test]
    fn a_model_list_failure_carries_the_status_and_body() {
        let server = crate::http::support::Server::new(vec![crate::http::support::reply(
            "401 Unauthorized",
            &[],
            "bad key",
        )]);
        let err = list_models(&server.ep, "/models", None, &[]).err().unwrap();
        assert!(err.contains("HTTP 401"), "{err}");
        assert!(err.contains("bad key"), "{err}");

        let server = crate::http::support::Server::new(vec![crate::http::support::json("nope")]);
        let err = list_models(&server.ep, "/models", None, &[]).err().unwrap();
        assert!(err.contains("models json"), "{err}");
    }

    #[test]
    fn a_chat_request_carries_the_level_fields_and_asks_for_usage() {
        let server =
            crate::http::support::Server::new(vec![crate::http::support::sse(&["[DONE]"])]);
        let cancel = Cancel::new();
        let extra = serde_json::json!({ "reasoning_effort": "high" });
        let res = chat(
            &server.ep,
            Some("k"),
            &[],
            "local/qwen3",
            &[serde_json::json!({"role":"user","content":"hi"})],
            &serde_json::json!([]),
            Some(&extra),
            &cancel,
        )
        .unwrap();
        assert_eq!(res.status, 200);

        let sent = server.text();
        let body = sent.split("\r\n\r\n").nth(1).unwrap_or_default();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "local/qwen3");
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(sent.starts_with("POST /chat/completions "), "{sent:?}");
    }

    #[test]
    fn a_streamed_reply_arrives_through_the_sockets() {
        let server = crate::http::support::Server::new(vec![crate::http::support::sse(&[
            r#"{"choices":[{"delta":{"reasoning":"th"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"ink"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ans"}}]}"#,
            r#"{"choices":[{"delta":{"content":"wer"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
            r#"{"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "[DONE]",
        ])]);
        let cancel = Cancel::new();
        let mut text = String::new();
        let mut thought = String::new();
        let reply = stream_chat(
            &server.ep,
            None,
            &[],
            "m",
            &[],
            &serde_json::json!([]),
            None,
            &cancel,
            |part| text.push_str(part),
            |part| thought.push_str(part),
        )
        .unwrap();
        assert_eq!(text, "answer");
        assert_eq!(thought, "think");
        assert_eq!(reply.text, "answer");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].id, "c1");
        assert_eq!(reply.calls[0].arguments, "{\"path\":\"x\"}");
        assert!(reply.complete, "the stream ended with [DONE]");
        assert!(reply.cut().is_none());
        assert_eq!(reply.usage.as_ref().unwrap().prompt, 5);
    }

    #[test]
    fn a_stream_without_done_is_not_a_whole_reply() {
        // A close-delimited body ends with the connection: only `[DONE]` or a
        // finish reason says the answer is whole.
        let server = crate::http::support::Server::new(vec![crate::http::support::sse(&[
            r#"{"choices":[{"delta":{"content":"half"}}]}"#,
        ])]);
        let cancel = Cancel::new();
        let reply = stream_chat(
            &server.ep,
            None,
            &[],
            "m",
            &[],
            &serde_json::json!([]),
            None,
            &cancel,
            |_| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(reply.text, "half");
        assert!(!reply.complete);
        assert_eq!(reply.cut(), Some("the stream ended before the reply did"));

        // A reason on the choice ends the stream just as `[DONE]` does; the
        // reason is what may still say the answer was cut short.
        let mut whole = Reply {
            reasoning_field: None,
            text: "half".into(),
            reasoning: String::new(),
            calls: Vec::new(),
            usage: None,
            finish_reason: Some("stop".into()),
            complete: true,
        };
        assert!(whole.cut().is_none());
        whole.finish_reason = Some("length".into());
        assert_eq!(
            whole.cut(),
            Some("the reply hit the provider's length limit")
        );
    }

    #[test]
    fn a_streamed_error_chunk_fails_the_request() {
        let server = crate::http::support::Server::new(vec![crate::http::support::sse(&[
            r#"{"error":{"message":"rate limited"}}"#,
        ])]);
        let cancel = Cancel::new();
        let err = stream_chat(
            &server.ep,
            None,
            &[],
            "m",
            &[],
            &serde_json::json!([]),
            None,
            &cancel,
            |_| {},
            |_| {},
        )
        .err()
        .unwrap();
        assert!(err.contains("rate limited"), "{err}");
    }

    #[test]
    fn a_refused_chat_reports_the_status_and_body() {
        let server = crate::http::support::Server::new(vec![crate::http::support::reply(
            "403 Forbidden",
            &[],
            "no go",
        )]);
        let cancel = Cancel::new();
        let err = stream_chat(
            &server.ep,
            None,
            &[],
            "m",
            &[],
            &serde_json::json!([]),
            None,
            &cancel,
            |_| {},
            |_| {},
        )
        .err()
        .unwrap();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("no go"), "{err}");
    }
}
