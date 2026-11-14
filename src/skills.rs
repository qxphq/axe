//! Agent Skills: discovery, frontmatter, the prompt catalogue, and the body
//! read on demand.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where a skill was found. Project wins over global, global over an explicit
/// config path, and a nearer project root over a farther one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Project,
    Global,
    Config,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
            Scope::Config => "config",
        }
    }
}

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file: PathBuf,
    pub scope: Scope,
    /// `allowed-tools`, advisory: axe has no permission layer to pre-approve.
    pub allowed_tools: Vec<String>,
    /// `disable-model-invocation: true`; `/skill:name` still loads it.
    pub hidden: bool,
}

pub struct Loaded {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
}

/// One skill costs roughly 100 tokens in the prompt; the cap keeps a runaway
/// tree from flooding the prefix.
const MAX_SKILLS: usize = 200;
const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// A SKILL.md body larger than this is cut; the recommended size is far lower.
const MAX_BODY_BYTES: usize = 48 * 1024;
/// Only the frontmatter is read at startup; it has to live in this window.
const HEAD_BYTES: u64 = 64 * 1024;
const MAX_DEPTH: usize = 6;
const MAX_DIRS: usize = 2000;

const SKIP: &[&str] = &[".git", "node_modules", "target"];

/// Scan the default locations plus the config's `skills` paths.
pub fn load(cwd: Option<&Path>, extra: &[PathBuf]) -> Loaded {
    let mut roots = Vec::new();
    if let Some(cwd) = cwd {
        for dir in project_roots(cwd) {
            roots.push((dir, Scope::Project));
        }
    }
    if let Some(dir) = crate::config::path().parent() {
        roots.push((dir.join("skills"), Scope::Global));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        roots.push((PathBuf::from(home).join(".agents/skills"), Scope::Global));
    }
    for path in extra.iter().filter(|path| !path.as_os_str().is_empty()) {
        roots.push((path.clone(), Scope::Config));
    }

    let mut loaded = scan(&roots);
    for path in extra.iter().filter(|path| !path.as_os_str().is_empty()) {
        if !path.exists() {
            loaded
                .warnings
                .push(format!("skills: {} does not exist", path.display()));
        }
    }
    loaded
}

/// `.agents/skills` at the working directory and each ancestor, stopping at
/// the repository root. Without a `.git`, only the working directory counts.
fn project_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut at_repo = false;
    for dir in cwd.ancestors() {
        roots.push(dir.join(".agents/skills"));
        if dir.join(".git").exists() {
            at_repo = true;
            break;
        }
    }
    if !at_repo {
        roots.truncate(1);
    }
    roots
}

/// Roots in precedence order. Split from `load` so tests need no home
/// directory or XDG config.
pub fn scan(roots: &[(PathBuf, Scope)]) -> Loaded {
    let mut scan = Scan::default();
    for (dir, scope) in roots {
        scan.add_root(dir, *scope);
    }
    if scan.capped {
        scan.warnings
            .push(format!("skills: at most {MAX_SKILLS} are listed"));
    }
    if scan.dirs_capped {
        scan.warnings.push(format!(
            "skills: stopped scanning after {MAX_DIRS} directories"
        ));
    }
    Loaded {
        skills: scan.skills,
        warnings: scan.warnings,
    }
}

#[derive(Default)]
struct Scan {
    skills: Vec<Skill>,
    warnings: Vec<String>,
    files: HashSet<PathBuf>,
    dirs: usize,
    capped: bool,
    dirs_capped: bool,
}

impl Scan {
    fn add_root(&mut self, path: &Path, scope: Scope) {
        if path.is_file() {
            if let Some(skill) = self.skill(path, scope) {
                self.merge(skill);
            }
            return;
        }
        let mut found = Vec::new();
        self.walk(path, scope, 0, &mut found);
        for skill in found {
            self.merge(skill);
        }
    }

    /// Keep the first skill per name and per real path. A project skill that
    /// shares a name with a global one replaces it, because project roots are
    /// scanned first.
    fn merge(&mut self, skill: Skill) {
        let key = fs::canonicalize(&skill.file).unwrap_or_else(|_| skill.file.clone());
        if !self.files.insert(key) {
            return;
        }
        if let Some(kept) = self.skills.iter().find(|kept| kept.name == skill.name) {
            self.warnings.push(format!(
                "skill \"{}\": {} shadowed by {}",
                skill.name,
                skill.file.display(),
                kept.file.display()
            ));
            return;
        }
        if self.skills.len() >= MAX_SKILLS {
            self.capped = true;
            return;
        }
        self.skills.push(skill);
    }

    /// A directory holding `SKILL.md` is a skill root: its siblings are that
    /// skill's files, not more skills.
    fn walk(&mut self, dir: &Path, scope: Scope, depth: usize, out: &mut Vec<Skill>) {
        if !dir.is_dir() || depth > MAX_DEPTH || self.dirs >= MAX_DIRS {
            if self.dirs >= MAX_DIRS {
                self.dirs_capped = true;
            }
            return;
        }
        self.dirs += 1;
        let file = dir.join("SKILL.md");
        if file.is_file() {
            if out.len() < MAX_SKILLS {
                if let Some(skill) = self.skill(&file, scope) {
                    out.push(skill);
                }
            }
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    self.warnings
                        .push(format!("skills: {}: {e}", dir.display()));
                }
                return;
            }
        };
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            // A symlinked directory is not followed: cycles and scans that
            // leave the tree are not worth a path set here.
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
                _ => {}
            }
        }
        dirs.sort();
        for child in dirs {
            self.walk(&child, scope, depth + 1, out);
        }
    }

    /// Parse one `SKILL.md`. Missing or unreadable frontmatter and a missing
    /// description skip the skill; cosmetic name problems only warn.
    fn skill(&mut self, file: &Path, scope: Scope) -> Option<Skill> {
        let text = match read_head(file) {
            Ok(text) => text,
            Err(e) => {
                self.warnings.push(format!("skill {}: {e}", file.display()));
                return None;
            }
        };
        let Some((front, _)) = split_frontmatter(&text) else {
            self.warnings.push(format!(
                "skill {}: frontmatter is missing or unclosed; skipped",
                file.display()
            ));
            return None;
        };
        let dir = file.parent().unwrap_or(Path::new("."));
        let dir_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let declared = front.name.unwrap_or_default().trim().to_string();
        let name = if declared.is_empty() {
            dir_name.clone()
        } else {
            declared
        };
        if name.is_empty() {
            self.warnings.push(format!(
                "skill {}: no name and no directory name",
                file.display()
            ));
            return None;
        }
        let Some(description) = front
            .description
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            self.warnings.push(format!(
                "skill {}: description is required; skipped",
                file.display()
            ));
            return None;
        };
        for problem in name_problems(&name) {
            self.warnings
                .push(format!("skill {}: {problem}", file.display()));
        }
        if !dir_name.is_empty() && name != dir_name {
            self.warnings.push(format!(
                "skill {}: name \"{name}\" does not match its directory \"{dir_name}\"",
                file.display()
            ));
        }
        Some(Skill {
            name,
            description,
            file: file.to_path_buf(),
            scope,
            allowed_tools: front
                .allowed_tools
                .map(|text| text.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            hidden: front.hidden,
        })
    }
}

fn read_head(file: &Path) -> std::io::Result<String> {
    let file = fs::File::open(file)?;
    let mut bytes = Vec::new();
    file.take(HEAD_BYTES).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn name_problems(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if name.chars().count() > 64 {
        problems.push(format!("name \"{name}\" is longer than 64 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        problems.push(format!(
            "name \"{name}\" must be lowercase letters, digits and hyphens"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        problems.push(format!(
            "name \"{name}\" must not start, end or repeat a hyphen"
        ));
    }
    problems
}

#[derive(Default)]
struct Front {
    name: Option<String>,
    description: Option<String>,
    allowed_tools: Option<String>,
    hidden: bool,
}

/// Split `---` frontmatter from the body. `None` when the fence is missing or
/// never closed, which is the one error that skips a skill outright.
fn split_frontmatter(text: &str) -> Option<(Front, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut head = Vec::new();
    let mut body = String::new();
    let mut closed = false;
    for line in lines {
        if closed {
            body.push_str(line);
            body.push('\n');
        } else if line.trim_end() == "---" {
            closed = true;
        } else {
            head.push(line);
        }
    }
    if !closed {
        return None;
    }
    Some((parse_head(&head), body))
}

/// The YAML subset skills use in practice: top-level `key: value`, plain or
/// quoted scalars, and folded/literal block scalars. Unknown keys, nested
/// maps, comments and chomping indicators are ignored, not rejected: other
/// clients accept this frontmatter and axe must load the same files.
fn parse_head(lines: &[&str]) -> Front {
    let mut front = Front::default();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim().is_empty() || line.starts_with([' ', '\t']) {
            index += 1;
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let key = key.trim();
        let rest = strip_comment(rest.trim());
        let (value, next) = if matches!(rest, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let folded = rest.starts_with('>');
            let mut block = Vec::new();
            let mut next = index + 1;
            while next < lines.len() {
                let line = lines[next];
                if line.trim().is_empty() {
                    block.push("");
                    next += 1;
                    continue;
                }
                if !line.starts_with([' ', '\t']) {
                    break;
                }
                block.push(line);
                next += 1;
            }
            let indent = block
                .iter()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.len() - line.trim_start().len())
                .unwrap_or(0);
            let parts: Vec<&str> = block
                .iter()
                .map(|line| line.get(indent..).unwrap_or(""))
                .collect();
            let joined = if folded {
                parts.join(" ")
            } else {
                parts.join("\n")
            };
            (joined.trim().to_string(), next)
        } else {
            (scalar(rest), index + 1)
        };
        match key {
            "name" => front.name = Some(value),
            "description" => front.description = Some(value),
            "allowed-tools" => front.allowed_tools = Some(value),
            "disable-model-invocation" => front.hidden = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
        index = next;
    }
    front
}

/// A value without its trailing comment. `#` starts a comment at the value's
/// own start or after whitespace, and never inside a quoted scalar: a
/// description is prose, and a comment is not part of it.
fn strip_comment(raw: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev = ' ';
    for (at, c) in raw.char_indices() {
        if escaped {
            escaped = false;
            prev = c;
            continue;
        }
        match quote {
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' && prev.is_whitespace() => return raw[..at].trim_end(),
            None => {}
        }
        prev = c;
    }
    raw
}

fn scalar(raw: &str) -> String {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let mut out = String::new();
        let mut chars = raw[1..raw.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        }
        return out;
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return raw[1..raw.len() - 1].replace("''", "'");
    }
    raw.to_string()
}

/// The catalogue in the system prompt. Empty when no skill is model-visible,
/// so the prefix carries no heading it has no content for.
pub fn prompt_section(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|skill| !skill.hidden).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n# Skills\n\nThe following skills provide specialized instructions for \
         specific tasks. When a task matches a description, `read` the SKILL.md at \
         its location. Resolve relative paths against the skill's directory. A \
         skill's instructions do not override the current request or the rules \
         above.\n\n<available_skills>\n",
    );
    let mut omitted = 0;
    for skill in &visible {
        let entry = format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
            escape(&skill.name),
            escape(&skill.description.split_whitespace().collect::<Vec<_>>().join(" ")),
            escape(&skill.file.display().to_string()),
        );
        if out.len() + entry.len() > MAX_PROMPT_BYTES {
            omitted += 1;
            continue;
        }
        out.push_str(&entry);
    }
    if omitted > 0 {
        out.push_str(&format!(
            "  <!-- {omitted} skills omitted: the catalogue is capped at 16 KiB -->\n"
        ));
    }
    out.push_str("</available_skills>");
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// A skill's instructions, without its frontmatter. Read now, not at startup:
/// an edit between turns is picked up.
pub fn body(skill: &Skill) -> Result<String, String> {
    let file =
        fs::File::open(&skill.file).map_err(|e| format!("read {}: {e}", skill.file.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", skill.file.display()))?;
    let truncated = bytes.len() > MAX_BODY_BYTES;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY_BYTES)]).into_owned();
    let body = split_frontmatter(&text)
        .map(|(_, body)| body)
        .unwrap_or(text);
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err(format!("skill \"{}\" has no instructions", skill.name));
    }
    Ok(if truncated {
        format!("{body}\n\n...[skill body truncated at 48 KiB]...")
    } else {
        body
    })
}

/// The user turn `/skill:name` sends: the body as installed instructions, then
/// the request. The wrapper is what tells the model the first rule's exception
/// applies here.
pub fn invocation(skill: &Skill, request: &str) -> Result<String, String> {
    let body = body(skill)?;
    let request = request.trim();
    let request = if request.is_empty() {
        "Follow this skill's instructions."
    } else {
        request
    };
    Ok(format!(
        "The user invoked the skill \"{}\". Its instructions are below; the current \
         request still wins.\n\n<skill name=\"{}\" path=\"{}\">\n{}\n</skill>\n\n{}",
        skill.name,
        escape(&skill.name),
        escape(&skill.file.display().to_string()),
        body,
        request
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp(name: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "axe-test-skills-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, front: &str, body: &str) -> PathBuf {
        let skill = dir.join(name);
        fs::create_dir_all(&skill).unwrap();
        let file = skill.join("SKILL.md");
        fs::write(&file, format!("---\n{front}\n---\n\n{body}\n")).unwrap();
        file
    }

    #[test]
    fn frontmatter_accepts_plain_quoted_and_block_values() {
        let (front, body) = split_frontmatter(
            "---\nname: pdf-tools\ndescription: Use this skill when: the user asks about PDFs\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(front.name.as_deref(), Some("pdf-tools"));
        assert_eq!(
            front.description.as_deref(),
            Some("Use this skill when: the user asks about PDFs")
        );
        assert_eq!(body, "body\n");

        let (front, _) =
            split_frontmatter("---\nname: \"quoted\"\ndescription: >\n  folded\n  text\n---\n")
                .unwrap();
        assert_eq!(front.name.as_deref(), Some("quoted"));
        assert_eq!(front.description.as_deref(), Some("folded text"));

        assert!(split_frontmatter("no frontmatter").is_none());
        assert!(split_frontmatter("---\nname: x\n").is_none());
    }

    #[test]
    fn skills_need_a_description_and_fall_back_to_the_directory() {
        let dir = temp("names");
        write_skill(
            &dir,
            "described",
            "name: described\ndescription: does things",
            "body",
        );
        write_skill(&dir, "fallback", "description: uses the dir name", "body");
        write_skill(&dir, "missing", "name: missing", "body");
        let loaded = scan(&[(dir.clone(), Scope::Project)]);
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"described"));
        assert!(names.contains(&"fallback"));
        assert!(!names.contains(&"missing"));
        assert!(loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("description is required")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_wins_over_global_and_the_first_name_kept() {
        let project = temp("project");
        let global = temp("global");
        write_skill(&project, "shared", "description: project version", "body");
        write_skill(&global, "shared", "description: global version", "body");
        write_skill(&global, "only-global", "description: global only", "body");
        let loaded = scan(&[
            (project.clone(), Scope::Project),
            (global.clone(), Scope::Global),
        ]);
        assert_eq!(loaded.skills.len(), 2);
        let shared = loaded
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .unwrap();
        assert_eq!(shared.description, "project version");
        assert!(loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("shadowed")));
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&global);
    }

    #[test]
    fn a_project_skill_loads_like_any_other() {
        let dir = temp("project-skill");
        write_skill(&dir, "repo-skill", "description: from the repo", "body");
        let loaded = scan(&[(dir.clone(), Scope::Project)]);
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].scope, Scope::Project);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recursion_stops_at_a_skill_root_and_skips_heavy_directories() {
        let dir = temp("walk");
        let nested = dir.join("group/nested");
        fs::create_dir_all(nested.join("scripts")).unwrap();
        fs::write(
            nested.join("SKILL.md"),
            "---\ndescription: nested skill\n---\nbody\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("node_modules/hidden")).unwrap();
        fs::write(
            dir.join("node_modules/hidden/SKILL.md"),
            "---\ndescription: never loaded\n---\nbody\n",
        )
        .unwrap();
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["nested"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_path_may_be_a_skill_file() {
        let dir = temp("file");
        let file = write_skill(&dir, "single", "description: one file", "body");
        let loaded = scan(&[(file, Scope::Config)]);
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].scope, Scope::Config);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_and_allowed_tools_are_parsed_and_catalogued() {
        let dir = temp("catalogue");
        write_skill(
            &dir,
            "visible",
            "description: <use> & \"read\"\nallowed-tools: Bash(git:*) Read",
            "body",
        );
        write_skill(
            &dir,
            "manual",
            "description: hidden\ndisable-model-invocation: true",
            "body",
        );
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        assert_eq!(loaded.skills.len(), 2);
        let visible = loaded
            .skills
            .iter()
            .find(|skill| skill.name == "visible")
            .unwrap();
        assert_eq!(visible.allowed_tools, vec!["Bash(git:*)", "Read"]);
        let section = prompt_section(&loaded.skills);
        assert!(section.contains("&lt;use&gt; &amp; &quot;read&quot;"));
        assert!(section.contains(&visible.file.display().to_string()));
        assert!(!section.contains("<name>manual</name>"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_body_is_read_without_frontmatter_and_wrapped_for_invocation() {
        let dir = temp("body");
        let file = write_skill(
            &dir,
            "demo",
            "name: demo\ndescription: demo skill",
            "Follow these steps.",
        );
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        let skill = &loaded.skills[0];
        assert_eq!(body(skill).unwrap(), "Follow these steps.");
        let text = invocation(skill, "do it").unwrap();
        assert!(text.contains("<skill name=\"demo\" path=\""));
        assert!(text.contains("Follow these steps."));
        assert!(text.ends_with("do it"));
        assert!(file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_names_warn_but_still_load() {
        let dir = temp("invalid");
        let skill = dir.join("Bad_Name");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: Bad_Name\ndescription: still works\n---\nbody\n",
        )
        .unwrap();
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        assert_eq!(loaded.skills.len(), 1);
        assert!(loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("lowercase letters")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_roots_stop_at_the_repository() {
        let dir = temp("roots");
        let repo = dir.join("repo");
        let deep = repo.join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        // Without a `.git` anywhere above, only the working directory counts.
        assert_eq!(project_roots(&deep), vec![deep.join(".agents/skills")]);
        fs::create_dir_all(repo.join(".git")).unwrap();
        let roots = project_roots(&deep);
        assert_eq!(roots.len(), 4, "{roots:?}");
        assert_eq!(roots[0], deep.join(".agents/skills"));
        assert_eq!(roots[3], repo.join(".agents/skills"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_project_config_home_and_extra_roots() {
        use crate::state::testenv;

        testenv::home("skills-load", |home| {
            let repo = home.join("repo");
            let cwd = repo.join("sub");
            fs::create_dir_all(repo.join(".git")).unwrap();
            fs::create_dir_all(&cwd).unwrap();
            write_skill(
                &cwd.join(".agents/skills"),
                "project-skill",
                "description: from the repo",
                "body",
            );
            write_skill(
                &home.join("config/axe/skills"),
                "config-skill",
                "description: from the config",
                "body",
            );
            write_skill(
                &home.join(".agents/skills"),
                "home-skill",
                "description: from the home directory",
                "body",
            );
            let extra = home.join("extra");
            write_skill(
                &extra,
                "extra-skill",
                "description: from the config key",
                "body",
            );

            let missing = home.join("nowhere");
            let loaded = load(Some(&cwd), &[extra, missing.clone()]);
            let found: Vec<(&str, Scope)> = loaded
                .skills
                .iter()
                .map(|skill| (skill.name.as_str(), skill.scope))
                .collect();
            assert!(
                found.contains(&("project-skill", Scope::Project)),
                "{found:?}"
            );
            assert!(
                found.contains(&("config-skill", Scope::Global)),
                "{found:?}"
            );
            assert!(found.contains(&("home-skill", Scope::Global)), "{found:?}");
            assert!(found.contains(&("extra-skill", Scope::Config)), "{found:?}");
            assert!(
                loaded
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("does not exist")),
                "{:?}",
                loaded.warnings
            );
            // No working directory: no project roots, and nothing to fail on.
            let bare = load(None, &[]);
            assert!(!bare
                .skills
                .iter()
                .any(|skill| skill.name == "project-skill"));
        });
    }

    #[test]
    fn frontmatter_reads_the_subset_it_documents() {
        let text = "---\nunknown: value\n\nname: 'it''s named'\n\
                    description: \"quoted\\nwith\\ttab\\q\"\n\
                    allowed-tools: |\n  bash\n\n  read\ndisable-model-invocation: TRUE\n\
                    ---\nbody\n";
        let (front, body) = split_frontmatter(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("it's named"));
        assert_eq!(front.description.as_deref(), Some("quoted\nwith\ttabq"));
        assert_eq!(front.allowed_tools.as_deref(), Some("bash\n\nread"));
        assert!(front.hidden);
        assert_eq!(body, "body\n");
        // A folded block ends at the next top-level key.
        let (front, _) =
            split_frontmatter("---\ndescription: >-\n  folded\n  text\nname: after\n---\n")
                .unwrap();
        assert_eq!(front.description.as_deref(), Some("folded text"));
        assert_eq!(front.name.as_deref(), Some("after"));
        // A line with no colon is skipped, not fatal.
        let (front, _) = split_frontmatter("---\nno colon here\nname: x\n---\n").unwrap();
        assert_eq!(front.name.as_deref(), Some("x"));
        assert_eq!(scalar("plain text"), "plain text");
        assert_eq!(scalar("\"a\\\"b\""), "a\"b");
        assert_eq!(scalar("'single'"), "single");
    }

    /// A comment is YAML, not part of the value: a skill written the way YAML
    /// allows keeps the name its directory has.
    #[test]
    fn a_trailing_comment_is_not_part_of_the_value() {
        let text = "---\nname: pdf-tools # the reader\n\
                    description: Extracts text. # see the docs\n\
                    allowed-tools: read bash # both\ndisable-model-invocation: true # off\n---\n";
        let (front, _) = split_frontmatter(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("pdf-tools"));
        assert_eq!(front.description.as_deref(), Some("Extracts text."));
        assert_eq!(front.allowed_tools.as_deref(), Some("read bash"));
        assert!(front.hidden);
        // A block scalar's indicator takes one too.
        let (front, _) =
            split_frontmatter("---\ndescription: | # note\n  two\n  lines\n---\n").unwrap();
        assert_eq!(front.description.as_deref(), Some("two\nlines"));
        // A `#` inside quotes is content, and one with no space before it is
        // not a comment.
        assert_eq!(strip_comment("\"a # b\""), "\"a # b\"");
        assert_eq!(strip_comment("'a # b' # c"), "'a # b'");
        assert_eq!(strip_comment("\"a\\\" # b\""), "\"a\\\" # b\"");
        assert_eq!(strip_comment("a#b"), "a#b");
        assert_eq!(strip_comment("# all of it"), "");
    }

    #[test]
    fn a_name_is_checked_against_each_rule() {
        assert!(name_problems("pdf-tools").is_empty());
        let long = "a".repeat(65);
        let problems = name_problems(&long);
        assert!(
            problems.iter().any(|p| p.contains("longer than 64")),
            "{problems:?}"
        );
        let problems = name_problems("Bad_Name");
        assert!(
            problems.iter().any(|p| p.contains("lowercase")),
            "{problems:?}"
        );
        for name in ["-lead", "trail-", "two--hyphens"] {
            let problems = name_problems(name);
            assert!(
                problems.iter().any(|p| p.contains("hyphen")),
                "{name}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_body_may_be_empty_missing_or_huge() {
        let dir = temp("bodies");
        let file = write_skill(&dir, "empty", "description: nothing to say", "");
        let mut loaded = scan(&[(dir.clone(), Scope::Global)]);
        let err = body(&loaded.skills[0]).err().unwrap();
        assert!(err.contains("no instructions"), "{err}");

        // Frontmatter only: the body is empty too.
        fs::write(&file, "---\nname: empty\ndescription: x\n---\n").unwrap();
        loaded = scan(&[(dir.clone(), Scope::Global)]);
        assert!(body(&loaded.skills[0]).is_err());

        // No frontmatter at all: the skill is skipped before its body is read.
        let bare = dir.join("bare");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join("SKILL.md"), "steps\n").unwrap();
        loaded = scan(&[(dir.clone(), Scope::Global)]);
        assert!(!loaded.skills.iter().any(|skill| skill.name == "bare"));

        // A body over the cap is cut and says so.
        let huge = dir.join("huge");
        fs::create_dir_all(&huge).unwrap();
        let mut text = String::from("---\ndescription: huge\n---\n");
        text.push_str(&"x".repeat(MAX_BODY_BYTES + 10));
        fs::write(huge.join("SKILL.md"), text).unwrap();
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        let skill = loaded.skills.iter().find(|s| s.name == "huge").unwrap();
        let out = body(skill).unwrap();
        assert!(
            out.ends_with("[skill body truncated at 48 KiB]..."),
            "{out:.80}"
        );
        assert!(out.len() > MAX_BODY_BYTES, "the cap plus the note");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invocation_with_no_request_asks_for_the_instructions() {
        let dir = temp("invocation");
        write_skill(&dir, "demo", "description: demo", "Do the thing.");
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        let text = invocation(&loaded.skills[0], "   ").unwrap();
        assert!(
            text.ends_with("Follow this skill's instructions."),
            "{text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_directory_is_not_followed() {
        let dir = temp("symlink");
        let real = dir.join("outside");
        write_skill(&real, "real", "description: reached directly", "body");
        fs::create_dir_all(dir.join("scan")).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("scan/link")).unwrap();
        fs::create_dir_all(dir.join("scan/.hidden")).unwrap();
        fs::write(
            dir.join("scan/.hidden/SKILL.md"),
            "---\ndescription: hidden dir\n---\nbody\n",
        )
        .unwrap();
        let loaded = scan(&[(dir.join("scan"), Scope::Global)]);
        assert!(loaded.skills.is_empty(), "{} skills", loaded.skills.len());
        // The same skill loads when its directory is named, not linked.
        let loaded = scan(&[(real, Scope::Global)]);
        assert_eq!(loaded.skills.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_skill_file_warns_and_is_skipped() {
        let dir = temp("unreadable");
        let file = write_skill(&dir, "broken", "description: cannot be read", "body");
        fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let loaded = scan(&[(dir.clone(), Scope::Global)]);
        assert!(loaded.skills.is_empty());
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning.contains("skill")),
            "{:?}",
            loaded.warnings
        );
        fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skill_without_a_directory_name_or_description_is_skipped() {
        let dir = temp("skips");
        // A SKILL.md at the root of the scanned tree has no skill directory.
        let root = dir.join("scan");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\ndescription: at the root\n---\nbody\n",
        )
        .unwrap();
        // A separate root: a directory holding SKILL.md is not descended into.
        let other = dir.join("other");
        fs::create_dir_all(other.join("unclosed")).unwrap();
        fs::write(other.join("unclosed/SKILL.md"), "---\nname: x\n").unwrap();
        let loaded = scan(&[(root.clone(), Scope::Global), (other, Scope::Global)]);
        // A SKILL.md at the root of a scanned tree takes that directory's name.
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].name, "scan");
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning.contains("frontmatter is missing or unclosed")),
            "{:?}",
            loaded.warnings
        );
        // The catalogue of nothing is nothing: no heading, no empty block.
        assert_eq!(prompt_section(&[]), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
