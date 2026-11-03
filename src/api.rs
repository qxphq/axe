use serde_json::{json, Value};

use crate::http::{self, Endpoint, Response};

pub struct ModelInfo {
    pub id: String,
    pub context: Option<u64>,
}

pub fn list_models(ep: &Endpoint, api_key: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    let mut res = http::request(ep, "GET", "/models", api_key, None)?;
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

pub fn chat(
    ep: &Endpoint,
    api_key: Option<&str>,
    model: &str,
    messages: &[Value],
) -> Result<Response, String> {
    let body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
    })
    .to_string();
    http::request(ep, "POST", "/chat/completions", api_key, Some(&body))
}
