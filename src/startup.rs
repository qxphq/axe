//! The process bootstrap: config, providers, instructions and skills, the
//! splash, and the session the first line runs. Also what the interface says
//! about the model a session is on: the selection is made here, and the info
//! row and the choosers report it for the session's life.

use std::io::Read;

use ratatui::crossterm::terminal;
use serde_json::{json, Value};

use crate::api;
use crate::config;
use crate::http;
use crate::mcp;
use crate::prompt::{mode_note, system_with_instructions};
use crate::render::Paint;
use crate::sandbox;
use crate::sessions;
use crate::skills;
use crate::state;
use crate::store::{history_bytes, Store};
use crate::text::{plural, strip_control};
use crate::tools;
use crate::ui;
use crate::web;

use super::{Provider, Session};

pub(crate) fn run() -> Result<(), String> {
    let cfg = config::load()?;
    // `web_search` holds its token rather than the config: read once, like the
    // providers, and never re-read mid-session.
    web::set_brave_key(cfg.search.brave_key.clone());
    // Confinement is announced before anything runs, once, next to the
    // provider lines: weaker than promised must never be silent.
    if let Some(note) = sandbox::init() {
        eprintln!("axe: {}", strip_control(&note));
    }
    // Instructions are read once at startup, before any request: the system
    // prompt is a stable prefix and must not change mid-session.
    // Order: config `agents`, then the project's AGENTS.md, then `system`.
    let global_agents = cfg.agents.as_deref().and_then(config::read_agents);
    // The working directory identifies this directory's state file and anchors
    // the project's AGENTS.md; read it once.
    let cwd = std::env::current_dir().ok();
    let state = match &cwd {
        Some(dir) => state::load(dir),
        None => state::State::default(),
    };
    let project_file = cwd
        .as_deref()
        .and_then(config::find_agents)
        .filter(|f| Some(f.as_path()) != cfg.agents.as_deref());
    let project_agents = project_file.as_deref().and_then(config::read_agents);

    // Skills are read once, like the instructions: the catalogue is part of
    // the stable prefix.
    let loaded_skills = skills::load(cwd.as_deref(), &cfg.skills);
    // Explore mode's second writable directory, and the only one that outlives
    // the session: scratch is removed at exit, so a plan needs its own place.
    if let Some(dir) = &cwd {
        let plan = state::project(dir).join("plan");
        match state::private_dir(&plan) {
            Ok(()) => sandbox::set_plan(&plan),
            Err(e) => eprintln!("axe: plan directory: {e}"),
        }
    }
    for warning in &loaded_skills.warnings {
        eprintln!("axe: {}", strip_control(warning));
    }
    let project_skills = loaded_skills
        .skills
        .iter()
        .filter(|skill| skill.scope == skills::Scope::Project)
        .count();
    // Named once, next to the splash: which instructions and skills are in
    // force is part of the session's identity, not a warning.
    let mut notices = Vec::new();
    if let Some(file) = project_file.as_deref().filter(|_| project_agents.is_some()) {
        notices.push(format!(
            "Using project instructions from {}",
            file.display()
        ));
    }
    if project_skills > 0 {
        notices.push(format!(
            "Using {project_skills} project skill{}",
            plural(project_skills)
        ));
    }
    // The mode is history, not prompt: the config decides where a session
    // starts, and the note below tells the model which side it is on.
    let mode = if cfg.explore_by_default {
        tools::Mode::Explore
    } else {
        tools::Mode::Normal
    };
    // The directory's sessions: archive the old bodies before the listing so
    // the markers match what a chooser will resume. Read before the banner, so
    // a directory with no sessions gets the line that says where to start.
    let mut startup = Vec::new();
    if let Some(dir) = &cwd {
        sessions::archive_old(dir, sessions::KEEP);
        startup = sessions::list(dir);
        startup.truncate(15);
    }
    if startup.is_empty() {
        notices.push(first_run_note(mode).to_string());
    }
    // The splash comes before the first provider is asked for anything: it
    // needs nothing from one, and a provider that is slow or silent must not
    // leave the terminal blank. A piped run prints neither the splash, the
    // selection nor these notices: stdout is the transcript, and nobody asked
    // who is answering.
    if ui::is_interactive() {
        banner(&notices);
    }
    let mut providers = Vec::new();
    let sid = session_id();

    for pc in &cfg.providers {
        let ep = http::parse_url(&pc.base_url).map_err(|e| format!("{}: {e}", pc.name))?;
        let key = pc.resolved_key();
        if key.is_none() {
            if let Some(var) = &pc.api_key_env {
                eprintln!(
                    "{}: {} is not set, skipping",
                    strip_control(&pc.name),
                    strip_control(var)
                );
                continue;
            }
        }

        let mut headers = Vec::new();
        if let Some(name) = &pc.session_header {
            headers.push((name.clone(), sid.clone()));
        }

        let (models_ep, models_path) = match &pc.models_url {
            Some(url) => {
                let ep = http::parse_url(url).map_err(|e| format!("{}: {e}", pc.name))?;
                let path = if ep.prefix.is_empty() { "/models" } else { "" };
                (ep, path)
            }
            None => (ep.clone(), "/models"),
        };

        match api::list_models(&models_ep, models_path, key.as_deref(), &headers) {
            Ok(models) if !models.is_empty() => {
                // One line per provider as it answers, so a slow network reads
                // as work rather than as a dead terminal. A run without a
                // terminal says nothing here: stdout is the transcript.
                if ui::is_interactive() {
                    println!(
                        " {}",
                        Paint::Dim.ansi(&format!(
                            "{}: {} model{}",
                            strip_control(&pc.name),
                            models.len(),
                            plural(models.len())
                        ))
                    );
                }
                providers.push(Provider {
                    name: pc.name.clone(),
                    cfg: pc.clone(),
                    ep,
                    key,
                    headers,
                    models,
                });
            }
            Ok(_) => eprintln!("{}: no models", strip_control(&pc.name)),
            Err(e) => eprintln!("{}: {}", strip_control(&pc.name), strip_control(&e)),
        }
    }

    if providers.is_empty() {
        return Err("no models available".into());
    }

    // The directory's own choice wins, then the state root's default, then the
    // config's `model`, which is the seed for a machine that has picked none.
    let global = state::load_default();
    let (provider, model) = resolve(
        &providers,
        state
            .model
            .as_deref()
            .or(global.as_deref())
            .or(cfg.model.as_deref()),
    );
    // Thinking level follows the model: it is remembered per `provider/id`,
    // first for this directory, then for the config, and turned into request
    // fields by `thinking_body`.
    let spec = format!("{}/{}", providers[provider].name, model);
    let thinking = state.thinking_for(&spec).or_else(|| {
        cfg.thinking
            .iter()
            .find(|(s, _)| *s == spec)
            .map(|(_, level)| level.clone())
    });
    validate_level(&providers[provider].cfg, thinking.as_deref());

    // Built once. The mode lives in the history, not here, so a toggle leaves
    // the prefix byte-identical and the provider's cache intact. The project's
    // file is tagged: it is data about the repository, not authority.
    let skills_section = skills::prompt_section(&loaded_skills.skills);
    let system = system_with_instructions(
        global_agents.as_deref(),
        project_agents.as_deref().zip(project_file.as_deref()),
        cfg.system.as_deref(),
        Some(skills_section.as_str()).filter(|text| !text.is_empty()),
    );
    // MCP servers are read once, like the instructions: their tools join the
    // tool catalogue, and the catalogue is part of the request's cached prefix.
    // A project's file is code the repository shipped, so it runs only after
    // one approval; the prompt here is the only question a start asks.
    for line in mcp::start(cwd.as_deref(), ui::is_interactive(), mode.explore()) {
        if ui::is_interactive() {
            println!("{}", strip_control(&line));
        } else {
            eprintln!("axe: {}", strip_control(&line));
        }
    }
    let messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        mode_note(mode),
    ];

    let store = cwd
        .as_deref()
        .map(|dir| Store::fresh(sessions::dir(dir), &sid));

    let bytes = history_bytes(&messages);
    let session = Session {
        state,
        providers,
        provider,
        model,
        thinking,
        mode,
        used: None,
        messages,
        bytes,
        sid,
        store,
        startup,
        picks: Vec::new(),
        naming: None,
        warning: None,
        skills: loaded_skills.skills,
    };

    if ui::is_interactive() {
        session.run_interactive()
    } else {
        session.run_piped()
    }
}

pub(crate) fn thinking_label(provider: &config::ProviderConfig, level: &Option<String>) -> String {
    match level {
        Some(level) => format!("{level}  (levels: {})", level_names(provider).join(", ")),
        None => "default (not set)".to_string(),
    }
}

pub(crate) fn level_names(provider: &config::ProviderConfig) -> Vec<String> {
    let mut names: Vec<String> = provider.thinking.iter().map(|(k, _)| k.clone()).collect();
    if names.is_empty() {
        names.push("off".to_string());
    }
    names
}

/// Map user input onto a configured level name. Returns the canonical name, so
/// the config never stores an alias.
pub(crate) fn set_thinking(
    provider: &config::ProviderConfig,
    input: &str,
) -> Result<String, String> {
    let names = level_names(provider);
    names
        .iter()
        .find(|name| name.eq_ignore_ascii_case(input))
        .cloned()
        .ok_or_else(|| {
            format!(
                "unknown thinking level: {input} (known: {})",
                names.join(", ")
            )
        })
}

/// Warn once at startup if the config remembers a level the provider dropped.
pub(crate) fn validate_level(provider: &config::ProviderConfig, level: Option<&str>) {
    let Some(level) = level else { return };
    if !level_names(provider).iter().any(|name| name == level) {
        eprintln!(
            "axe: {}: thinking level \"{level}\" is not configured, using default",
            provider.name
        );
    }
}

/// `provider/model`, the pair that answers a turn.
pub(crate) fn place(providers: &[Provider], provider: usize, model: &str) -> String {
    format!("{}/{}", providers[provider].name, model)
}

/// The directory a session runs in, as the info row says it: the home
/// directory is written `~`, which is the one part of the path that never has
/// to be read twice.
pub(crate) fn dir_label(cwd: &std::path::Path) -> String {
    let text = cwd.display().to_string();
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let home = home.display().to_string();
    if home.is_empty() || !text.starts_with(&home) {
        return text;
    }
    match &text[home.len()..] {
        "" => "~".to_string(),
        // At a boundary: `/home/anonette` is not under `/home/anon`.
        rest if rest.starts_with('/') => format!("~{rest}"),
        _ => text,
    }
}

pub(crate) fn level(thinking: &Option<String>) -> String {
    thinking.clone().unwrap_or_else(|| "default".to_string())
}

/// The window as a whole percent, saturating: a count that overshoots the
/// window is full, not 140%.
pub(crate) fn fullness(used: u64, ctx: u64) -> u8 {
    (used.saturating_mul(100) / ctx.max(1)).min(100) as u8
}

/// Token counts in three characters where possible: `812`, `34k`, `1.0M`.
pub(crate) fn scaled(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Whole percent, with `<1%` for a history that is started but rounds to zero:
/// true on a million-token window, and `0%` would read as empty.
pub(crate) fn percent(used: u64, ctx: u64) -> String {
    let ctx = ctx.max(1);
    match used * 100 / ctx {
        0 if used > 0 => "<1%".to_string(),
        n => format!("{n}%"),
    }
}

/// Splash, printed once above the live region: block letters for `axe`. It
/// lands in the transcript and is never repainted, so it must be narrower than
/// the terminal's margin-to-margin width on a narrow window.
pub(crate) const LOGO: &[&str] = &[" ▄▄▄  ▄▄ ▄▄ ▄▄▄▄▄", "██▀██ ▀█▄█▀ ██▄▄", "██▀██ ██ ██ ██▄▄▄"];

/// What `/sessions` says where this directory has none, with a terminal and
/// without one: a session is what the first request starts, and the empty list
/// is the only place that has to say so.
pub(crate) const NO_SESSIONS: &str =
    "no sessions in this directory\n  one begins with the first request; /new starts another\n";

/// The one line a directory with no sessions gets. The box's colour and the
/// `read-only` piece say what the mode is; neither says where to start, and the
/// keys that open the rest of the interface are named nowhere else on screen. A
/// directory with sessions to resume gets the chooser instead, whose header
/// carries its own keys.
pub(crate) fn first_run_note(mode: tools::Mode) -> &'static str {
    if mode.explore() {
        "read-only: /explore to write · /help · ↑ history · Tab completes"
    } else {
        "/help · ↑ history · Tab completes"
    }
}

pub(crate) fn banner(notes: &[String]) {
    // One leading blank: the same margin `ui::Screen` keeps on both edges.
    println!();
    let width = terminal::size().map(|(width, _)| width).unwrap_or(0);
    let logo_width = LOGO
        .iter()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0);
    // The block logo must fit between the margins; a narrow terminal gets the
    // one-word mark instead of a wrapped banner.
    let logo: &[&str] = if width == 0 || width > logo_width {
        LOGO
    } else {
        &["axe"]
    };
    // The release rides the mark's second row; the one-word fallback has no
    // second row, so it takes the one it has.
    let version_row = (logo.len() - 1).min(1);
    for (i, line) in logo.iter().enumerate() {
        let mut out = Paint::Cyan.ansi(line);
        if i == version_row {
            out.push_str("   ");
            out.push_str(&Paint::Dim.ansi(env!("CARGO_PKG_VERSION")));
        }
        println!(" {}", out);
    }
    if !notes.is_empty() {
        println!();
        for note in notes {
            println!(" {}", Paint::Dim.ansi(note));
        }
    }
    println!();
}

pub(crate) fn session_id() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut bytes).is_ok() {
            return bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

pub(crate) fn models(providers: &[Provider]) -> String {
    let mut text = String::new();
    for p in providers {
        text.push_str(&format!("{}:\n", p.name));
        for m in &p.models {
            match m.context {
                Some(ctx) => text.push_str(&format!("  {}/{}  ({} ctx)\n", p.name, m.id, ctx)),
                None => text.push_str(&format!("  {}/{}\n", p.name, m.id)),
            }
        }
    }
    text
}

pub(crate) fn find(providers: &[Provider], spec: &str) -> Option<(usize, String)> {
    if let Some((pname, mid)) = spec.split_once('/') {
        if let Some(pi) = providers.iter().position(|p| p.name == pname) {
            if providers[pi].models.iter().any(|m| m.id == mid) {
                return Some((pi, mid.to_string()));
            }
        }
    }
    for (pi, p) in providers.iter().enumerate() {
        if p.models.iter().any(|m| m.id == spec) {
            return Some((pi, spec.to_string()));
        }
    }
    None
}

pub(crate) fn resolve(providers: &[Provider], spec: Option<&str>) -> (usize, String) {
    if let Some(spec) = spec {
        if let Some(hit) = find(providers, spec) {
            return hit;
        }
        eprintln!("axe: model {spec:?} not available, using default");
    }
    (0, providers[0].models[0].id.clone())
}
