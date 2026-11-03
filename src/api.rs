use serde_json::{json, Value};

use crate::http::{self, Endpoint, Response};

pub struct ModelInfo {
    pub id: String,
    pub context: Option<u64>,
}

pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
}

pub fn list_models(
    ep: &Endpoint,
    path: &str,
    api_key: Option<&str>,
    headers: &[(String, String)],
) -> Result<Vec<ModelInfo>, String> {
    let mut res = http::request(ep, "GET", path, api_key, headers, None)?;
    let body = res.read_all().map_err(|e| format!("read models: {e}"))?;
    if res.status != 200 {
        return Err(format!("models: HTTP {}: {}", res.status, body.trim()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("models json: {e}"))?;
    let data = v["data"]
        .as_array()
        .ok_or_else(|| "models: missing \"data\" array".to_string())?;

    let mut models = Vec::new();
    for entry in data {
        if let Some(id) = entry["id"].as_str() {
            let context = entry["context_length"]
                .as_u64()
                .or_else(|| entry["context_window"].as_u64());
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
pub fn chat(
    ep: &Endpoint,
    api_key: Option<&str>,
    headers: &[(String, String)],
    model: &str,
    messages: &[Value],
    tools: &Value,
    extra: Option<&Value>,
) -> Result<Response, String> {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "tools": tools,
    });
    if let (Some(Value::Object(extra)), Some(obj)) = (extra, body.as_object_mut()) {
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
    }
    let body = body.to_string();
    http::request(ep, "POST", "/chat/completions", api_key, headers, Some(&body))
}

pub fn stream_chat<F: FnMut(&str), G: FnMut()>(
    ep: &Endpoint,
    api_key: Option<&str>,
    headers: &[(String, String)],
    model: &str,
    messages: &[Value],
    tools: &Value,
    extra: Option<&Value>,
    mut on_text: F,
    mut on_reasoning: G,
) -> Result<Reply, String> {
    let mut res = chat(ep, api_key, headers, model, messages, tools, extra)?;
    if res.status != 200 {
        let body = res.read_all().unwrap_or_default();
        return Err(format!("HTTP {}: {}", res.status, body.trim()));
    }

    let mut text = String::new();
    let mut partials: Vec<(String, String, String)> = Vec::new();

    loop {
        let line = match res.read_line() {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => return Err(format!("read stream: {e}")),
        };
        let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if line.is_empty() {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        let delta = &chunk["choices"][0]["delta"];
        // Providers disagree on the key; any non-empty delta means the model is
        // still reasoning and no answer has started.
        let reasoning = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str());
        if reasoning.is_some_and(|r| !r.is_empty()) {
            on_reasoning();
        }
        if let Some(part) = delta["content"].as_str() {
            text.push_str(part);
            on_text(part);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                while partials.len() <= index {
                    partials.push((String::new(), String::new(), String::new()));
                }
                let partial = &mut partials[index];
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
    }

    let calls = partials
        .into_iter()
        .filter(|(_, name, _)| !name.is_empty())
        .map(|(id, name, arguments)| ToolCall {
            id,
            name,
            arguments,
        })
        .collect();

    Ok(Reply { text, calls })
}
