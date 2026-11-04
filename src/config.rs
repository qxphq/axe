//! The one config file: load, validate, resolve `agents`, and read provider
//! keys and thinking levels.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Clone)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
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
        let var = self.api_key_env.as_ref()?;
        env::var(var).ok().filter(|s| !s.is_empty())
    }
}

pub struct Search {
    /// Brave Search API token, resolved from `api_key_env`.
    /// `None` sends queries to DuckDuckGo.
    pub brave_key: Option<String>,
}

pub struct Config {
    pub providers: Vec<ProviderConfig>,
    pub model: Option<String>,
    /// Whether a session starts in explore mode (`read` and a read-only
    /// `bash`). On unless the config sets `"explore_by_default": false`.
    pub explore_by_default: bool,
    /// Web search: Brave when a token resolves, DuckDuckGo otherwise.
    pub search: Search,
    /// Model spec (`provider/id`) -> remembered thinking level.
    pub thinking: Vec<(String, String)>,
    pub system: Option<String>,
    pub agents: Option<PathBuf>,
}

pub fn path() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("axe/config.json");
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/axe/config.json")
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
  "search": {
    "brave": {
      "api_key_env": "BRAVE_SEARCH_API_KEY"
    }
  },
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
            eprintln!("axe: cannot read {}: {e}", file.display());
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }
    if text.len() > AGENTS_LIMIT {
        eprintln!(
            "axe: {} is {} bytes, truncated to {}",
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
    let base_url =
        text(v, "base_url").ok_or_else(|| format!("provider \"{name}\": missing \"base_url\""))?;
    if v.get("api_key").is_some() {
        return Err(format!(
            "provider \"{name}\": \"api_key\" is not supported; put the secret in the \
             environment and point \"api_key_env\" at it"
        ));
    }
    Ok(ProviderConfig {
        name: name.to_string(),
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key_env: text(v, "api_key_env"),
        models_url: text(v, "models_url"),
        session_header: text(v, "session_header"),
        thinking: v["thinking"]
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    })
}

pub fn load() -> Result<Config, String> {
    let p = path();
    let raw = match fs::read_to_string(&p) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = p.parent() {
                let _ = fs::create_dir_all(dir);
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
            // The example carries no secret, but the file it becomes holds
            // keys in some setups: owner-only from the first byte.
            let created = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&p)
                .and_then(|mut file| file.write_all(format!("{EXAMPLE}\n").as_bytes()));
            return match created {
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

    let v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("invalid JSON in {}: {e}", p.display()))?;
    parse_value(&v).map_err(|e| format!("{}: {e}", p.display()))
}

fn parse_value(v: &Value) -> Result<Config, String> {
    let mut providers = Vec::new();
    if let Some(map) = v["providers"].as_object() {
        for (name, entry) in map {
            providers.push(parse_provider(name, entry)?);
        }
    } else if let Some(base_url) = text(v, "base_url") {
        if v.get("api_key").is_some() {
            return Err(
                "\"api_key\" is not supported; put the secret in the environment \
                 and point \"api_key_env\" at it"
                    .into(),
            );
        }
        providers.push(ProviderConfig {
            name: "local".to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key_env: None,
            models_url: None,
            session_header: None,
            thinking: Vec::new(),
        });
    } else {
        return Err("need \"providers\" or \"base_url\"".into());
    }

    if providers.is_empty() {
        return Err("no providers configured".into());
    }

    // An unset key is not an error: search falls back to DuckDuckGo.
    let brave = &v["search"]["brave"];
    if brave.get("api_key").is_some() {
        return Err(
            "\"search.brave.api_key\" is not supported; put the secret in the \
             environment and point \"search.brave.api_key_env\" at it"
                .into(),
        );
    }
    let brave_key =
        text(brave, "api_key_env").and_then(|var| env::var(var).ok().filter(|s| !s.is_empty()));

    Ok(Config {
        providers,
        model: text(v, "model"),
        explore_by_default: v["explore_by_default"].as_bool().unwrap_or(true),
        search: Search { brave_key },
        thinking: v["thinking"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(k, l)| Some((k.clone(), l.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
        system: text(v, "system"),
        agents: text(v, "agents").map(|raw| agents_file(&raw)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let v: Value = serde_json::from_str(EXAMPLE).unwrap();
        let cfg = parse_value(&v).unwrap();
        assert!(cfg.providers.len() >= 3);
        assert_eq!(cfg.model.as_deref(), Some("local/qwen3.8-flash-next-chat"));
        assert!(cfg.explore_by_default);
        assert!(cfg
            .providers
            .iter()
            .any(|p| p.session_header.as_deref() == Some("x-opencode-session")));
    }

    #[test]
    fn top_level_base_url_makes_local_provider() {
        let cfg = parse_value(&serde_json::json!({
            "base_url": "http://local:8000/v1/",
            "explore_by_default": false,
            "system": "be brief"
        }))
        .unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "local");
        assert_eq!(cfg.providers[0].base_url, "http://local:8000/v1");
        assert!(!cfg.explore_by_default);
        assert_eq!(cfg.system.as_deref(), Some("be brief"));
    }

    #[test]
    fn config_rejects_missing_provider_keys() {
        assert!(parse_value(&serde_json::json!({})).is_err());
        assert!(parse_value(&serde_json::json!({
            "providers": {"p": {}}
        }))
        .is_err());
    }

    #[test]
    fn provider_level_and_key_helpers() {
        std::env::set_var("AXE_TEST_PROVIDER_KEY", "secret");
        let provider = ProviderConfig {
            name: "p".into(),
            base_url: "http://p".into(),
            api_key_env: Some("AXE_TEST_PROVIDER_KEY".into()),
            models_url: None,
            session_header: None,
            thinking: vec![
                ("off".into(), serde_json::json!({})),
                ("high".into(), serde_json::json!({"effort":"high"})),
            ],
        };
        assert_eq!(provider.resolved_key().as_deref(), Some("secret"));
        assert_eq!(
            provider.thinking_body(Some("high")),
            Some(serde_json::json!({"effort":"high"}))
        );
        assert_eq!(provider.thinking_body(Some("off")), None);
        assert_eq!(provider.thinking_body(Some("missing")), None);
        std::env::remove_var("AXE_TEST_PROVIDER_KEY");
        assert_eq!(provider.resolved_key(), None);
    }

    #[test]
    fn literal_api_keys_are_rejected() {
        assert!(parse_value(&serde_json::json!({
            "providers": {"p": {"base_url": "http://p", "api_key": "secret"}}
        }))
        .is_err());
        assert!(parse_value(&serde_json::json!({
            "providers": {"p": {"base_url": "http://p"}},
            "search": {"brave": {"api_key": "secret"}}
        }))
        .is_err());
        assert!(parse_value(&serde_json::json!({
            "base_url": "http://p",
            "api_key": "secret"
        }))
        .is_err());
    }

    #[test]
    fn agents_are_read_and_found_upwards() {
        let dir = std::env::temp_dir().join(format!("axe-test-agents-{}", std::process::id()));
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "rules").unwrap();
        assert_eq!(
            read_agents(&dir.join("AGENTS.md")).as_deref(),
            Some("rules")
        );
        assert!(find_agents(&nested).unwrap().ends_with("AGENTS.md"));
        assert!(read_agents(&dir.join("missing.md")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
