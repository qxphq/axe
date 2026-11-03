use std::env;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;

pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub models_url: Option<String>,
    pub session_header: Option<String>,
}

impl ProviderConfig {
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
    pub system: Option<String>,
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
    }
  },
  "model": "local/qwen3.8-flash-next-chat",
  "system": null
}"#;

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
    })
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
        system: text(&v, "system"),
    })
}
