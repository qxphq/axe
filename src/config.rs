use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub models_url: Option<String>,
    pub session_header: Option<String>,
    /// Level name -> JSON object merged into the request body.
    pub thinking: Vec<(String, Value)>,
}

impl ProviderConfig {
    /// Extra request-body fields for a thinking level. `None` means "let the
    /// provider default apply" — true for an unset level and for `off`.
    pub fn thinking_body(&self, level: Option<&str>) -> Option<Value> {
        let level = level?;
        let (_, body) = self.thinking.iter().find(|(name, _)| name == level)?;
        match body {
            Value::Object(map) if map.is_empty() => None,
            other => Some(other.clone()),
        }
    }

    pub fn resolved_key(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            return Some(key.clone());
        }
        let var = self.api_key_env.as_ref()?;
        env::var(var).ok().filter(|s| !s.is_empty())
    }
}

pub struct Config {
    pub providers: Vec<ProviderConfig>,
    pub model: Option<String>,
    /// Whether a session starts in explore mode (`read` and a read-only
    /// `bash`). On unless the config sets `"explore_by_default": false`.
    pub explore_by_default: bool,
    /// Model spec (`provider/id`) -> remembered thinking level.
    pub thinking: Vec<(String, String)>,
    pub system: Option<String>,
    pub agents: Option<PathBuf>,
}

pub fn path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("nano/config.json");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/nano/config.json")
}

const EXAMPLE: &str = r#"{
  "providers": {
    "local": {
      "base_url": "http://llm.local:8000/v1"
    },
    "deepseek": {
      "base_url": "https://api.deepseek.com",
      "api_key_env": "DEEPSEEK_API_KEY"
    },
    "opencode-go": {
      "base_url": "https://opencode.ai/inference/go/openai/v1",
      "models_url": "https://opencode.ai/inference/go/v1/models",
      "api_key_env": "OPENCODE_API_KEY",
      "session_header": "x-opencode-session"
    },
    "deepseek-thinking": {
      "base_url": "https://api.deepseek.com",
      "api_key_env": "DEEPSEEK_API_KEY",
      "thinking": {
        "off": {},
        "low": {"reasoning_effort": "low"},
        "medium": {"reasoning_effort": "medium"},
        "high": {"reasoning_effort": "high"}
      }
    }
  },
  "model": "local/qwen3.8-flash-next-chat",
  "explore_by_default": true,
  "thinking": {},
  "system": null,
  "agents": null
}"#;

const AGENTS_LIMIT: usize = 48 * 1024;

/// Read an AGENTS.md-style file. Best-effort: a missing file is silent, other
/// errors and truncation are warnings. Capped so one file cannot flood context.
pub fn read_agents(file: &Path) -> Option<String> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("nano: cannot read {}: {e}", file.display());
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }
    if text.len() > AGENTS_LIMIT {
        eprintln!(
            "nano: {} is {} bytes, truncated to {}",
            file.display(),
            text.len(),
            AGENTS_LIMIT
        );
        let mut cut = AGENTS_LIMIT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        return Some(text[..cut].to_string());
    }
    Some(text)
}

/// Nearest `AGENTS.md` at or above `dir`.
pub fn find_agents(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let file = d.join("AGENTS.md");
        if file.is_file() {
            return Some(file);
        }
        cur = d.parent();
    }
    None
}

/// Resolve the `agents` config key: `~` and `~/` are expanded, relative paths
/// are relative to the directory holding the config file.
fn agents_file(raw: &str) -> PathBuf {
    let home = || PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into()));
    if raw == "~" {
        return home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    let p = PathBuf::from(raw);
    if p.is_relative() {
        if let Some(dir) = path().parent() {
            return dir.join(p);
        }
    }
    p
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_provider(name: &str, v: &Value) -> Result<ProviderConfig, String> {
    let base_url = text(v, "base_url")
        .ok_or_else(|| format!("provider \"{name}\": missing \"base_url\""))?;
    Ok(ProviderConfig {
        name: name.to_string(),
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key: text(v, "api_key"),
        api_key_env: text(v, "api_key_env"),
        models_url: text(v, "models_url"),
        session_header: text(v, "session_header"),
        thinking: v["thinking"]
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    })
}

/// Rewrite the config file, preserving unknown keys. Formatting and comments
/// are not preserved.
fn edit(apply: impl Fn(&mut Value)) {
    let p = path();
    let raw = match fs::read_to_string(&p) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("nano: cannot save config to {}: {e}", p.display());
            return;
        }
    };
    let mut v: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("nano: cannot save config to {}: {e}", p.display());
            return;
        }
    };
    if !v.is_object() {
        eprintln!("nano: cannot save config to {}: not an object", p.display());
        return;
    }
    apply(&mut v);
    let text = match serde_json::to_string_pretty(&v) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("nano: cannot save config to {}: {e}", p.display());
            return;
        }
    };
    if let Err(e) = fs::write(&p, format!("{text}\n")) {
        eprintln!("nano: cannot save config to {}: {e}", p.display());
    }
}

/// Persist the selected model as `provider/id`.
pub fn save_model(spec: &str) {
    edit(|v| v["model"] = Value::String(spec.to_string()));
}

/// Remember the thinking level for `spec` (`provider/id`). `off` is stored
/// explicitly so that raising a provider's default level does not leak into
/// models the user already turned off.
pub fn save_thinking(spec: &str, level: &str) {
    edit(|v| {
        if !v["thinking"].is_object() {
            v["thinking"] = json!({});
        }
        v["thinking"][spec] = Value::String(level.to_string());
    });
}

pub fn load() -> Result<Config, String> {
    let p = path();
    let raw = match fs::read_to_string(&p) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
            }
            return match fs::write(&p, format!("{EXAMPLE}\n")) {
                Ok(()) => Err(format!(
                    "created default config at {}\nedit it and run again",
                    p.display()
                )),
                Err(e) => Err(format!(
                    "no config at {} ({e})\ncreate it with:\n{EXAMPLE}",
                    p.display()
                )),
            };
        }
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };

    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid JSON in {}: {e}", p.display()))?;

    let mut providers = Vec::new();
    if let Some(map) = v["providers"].as_object() {
        for (name, entry) in map {
            providers.push(parse_provider(name, entry)?);
        }
    } else if let Some(base_url) = text(&v, "base_url") {
        providers.push(ProviderConfig {
            name: "local".to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: text(&v, "api_key"),
            api_key_env: None,
            models_url: None,
            session_header: None,
            thinking: Vec::new(),
        });
    } else {
        return Err(format!(
            "{}: need \"providers\" or \"base_url\"",
            p.display()
        ));
    }

    if providers.is_empty() {
        return Err(format!("{}: no providers configured", p.display()));
    }

    Ok(Config {
        providers,
        model: text(&v, "model"),
        explore_by_default: v["explore_by_default"].as_bool().unwrap_or(true),
        thinking: v["thinking"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(k, l)| Some((k.clone(), l.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        system: text(&v, "system"),
        agents: text(&v, "agents").map(|raw| agents_file(&raw)),
    })
}
