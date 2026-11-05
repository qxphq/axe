//! Path completion for the prompt: the token at the cursor, listed against the
//! working directory as whole lines. The candidates replace the line the
//! prompt already has, so the existing list and its marker need to know
//! nothing about spans.
//!
//! `@` starts a path and is consumed by the completion: what lands on the line
//! is the path the model should read, not a sigil it has to interpret. A token
//! that is already a path — `~`, `/`, `.`, or anything with a `/` — completes
//! the same way without it. Anything else is prose, and Tab leaves it alone.

use std::fs;
use std::path::{Path, PathBuf};

/// Candidates listed at most: a directory of thousands is a prompt, not a list.
const LIMIT: usize = 100;

/// The lines Tab can put on the prompt for `line` at `cursor`, a char index.
pub fn paths(line: &str, cursor: usize) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    paths_in(line, cursor, &cwd, home.as_deref())
}

fn paths_in(line: &str, cursor: usize, cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let at = cursor.min(chars.len());
    // The token is the run of non-space the cursor is in or next to.
    let mut start = at;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = at;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    let forced = word.starts_with('@');
    let token = word.strip_prefix('@').unwrap_or(&word);
    if !forced && (token.is_empty() || !path_like(token)) {
        return Vec::new();
    }
    let head: String = chars[..start].iter().collect();
    let tail: String = chars[end..].iter().collect();
    let found = match token {
        // A bare `~` is the home directory itself: the rest is typed inside it.
        "~" => vec!["~/".to_string()],
        _ => list(token, cwd, home),
    };
    found
        .into_iter()
        .map(|path| format!("{head}{path}{tail}"))
        .collect()
}

/// Whether a token asks for a path rather than being prose.
fn path_like(token: &str) -> bool {
    token.starts_with('~')
        || token.starts_with('/')
        || token.starts_with('.')
        || token.contains('/')
}

/// The entries under `token`'s directory that start with its last segment.
/// Directories come first and carry the `/` that keeps the list going.
fn list(token: &str, cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let (shown, base) = match token.rfind('/') {
        Some(at) => (&token[..=at], &token[at + 1..]),
        None => ("", token),
    };
    let Ok(entries) = fs::read_dir(resolve(shown, cwd, home)) else {
        return Vec::new();
    };
    // The sort key is folded once per entry, not once per comparison: a
    // directory of thousands is a prompt, and this runs on every Tab.
    let mut matches: Vec<(bool, String, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Dotfiles are noise until the base itself asks for one.
        if !name.starts_with(base) || (name.starts_with('.') && !base.starts_with('.')) {
            continue;
        }
        matches.push((is_dir(&entry), name.to_lowercase(), name.to_string()));
    }
    matches.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    matches.truncate(LIMIT);
    matches
        .into_iter()
        .map(|(is_dir, _, name)| format!("{shown}{name}{}", if is_dir { "/" } else { "" }))
        .collect()
}

/// Whether the entry is a directory, following a symlink the way a `cd` would.
/// `file_type` is what the directory listing already knows; `metadata` is one
/// syscall per entry and only a link needs it.
fn is_dir(entry: &fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(kind) if kind.is_symlink() => fs::metadata(entry.path())
            .map(|meta| meta.is_dir())
            .unwrap_or(false),
        Ok(kind) => kind.is_dir(),
        Err(_) => false,
    }
}

/// What `shown` names on disk. The written form keeps `~/` as typed; only the
/// lookup expands it.
fn resolve(shown: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = shown.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    if shown.starts_with('/') {
        return PathBuf::from(shown);
    }
    cwd.join(shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("axe-test-paths-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(dir.join("src/ui.rs"), "").unwrap();
        fs::write(dir.join("src/app.rs"), "").unwrap();
        fs::write(dir.join("README.md"), "").unwrap();
        fs::write(dir.join(".hidden"), "").unwrap();
        dir
    }

    #[test]
    fn a_path_token_lists_its_directory_with_directories_first() {
        let dir = tree("list");
        assert_eq!(paths_in("src/ui", 6, &dir, None), vec!["src/ui.rs"]);
        assert_eq!(
            paths_in("@s", 2, &dir, None),
            vec!["src/".to_string()],
            "a bare name completes after @, and a directory keeps the list going"
        );
        assert_eq!(
            paths_in("@", 1, &dir, None),
            vec![
                "docs/".to_string(),
                "src/".to_string(),
                "README.md".to_string()
            ],
            "the @ is a trigger, not part of the path"
        );
        // Several matches stay in name order.
        let all = paths_in("src/", 4, &dir, None);
        assert_eq!(all, vec!["src/app.rs".to_string(), "src/ui.rs".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_entries_only_appear_when_asked_for() {
        let dir = tree("hidden");
        assert_eq!(paths_in(".h", 2, &dir, None), vec![".hidden"]);
        assert!(!paths_in("@", 1, &dir, None)
            .iter()
            .any(|path| path.contains(".hidden")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn absolute_and_home_paths_work() {
        let dir = tree("absolute");
        let absolute = format!("{}/src/", dir.display());
        let end = absolute.chars().count();
        assert_eq!(
            paths_in(&absolute, end, Path::new("/"), None),
            vec![format!("{absolute}app.rs"), format!("{absolute}ui.rs")]
        );
        assert_eq!(paths_in("~", 1, &dir, Some(&dir)), vec!["~/"]);
        assert_eq!(
            paths_in("~/src/a", 7, &dir, Some(&dir)),
            vec!["~/src/app.rs"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_keeps_what_is_around_the_token() {
        let dir = tree("around");
        // The cursor sits inside `src/ui`, and the tail survives.
        assert_eq!(
            paths_in("cp src/ui now", 8, &dir, None),
            vec!["cp src/ui.rs now"]
        );
        // Prose is not a path, and a missing directory is empty, not a panic.
        assert!(paths_in("hello", 5, &dir, None).is_empty());
        assert!(paths_in("nope/x", 6, &dir, None).is_empty());
        assert!(
            paths_in("", 0, &dir, None).is_empty(),
            "an empty line completes nothing"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_sort_case_insensitively_with_the_spelling_breaking_ties() {
        let dir = std::env::temp_dir().join(format!("axe-test-sort-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a_dir")).unwrap();
        for name in ["b.txt", "B.txt", "A.txt", "c.txt"] {
            fs::write(dir.join(name), "").unwrap();
        }
        // Directories first, then folded names, and `B` before `b` because that
        // is the exact order — the folded key must not shuffle them.
        assert_eq!(
            paths_in("@", 1, &dir, None),
            vec![
                "a_dir/".to_string(),
                "A.txt".to_string(),
                "B.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_directory_keeps_its_slash() {
        let dir = std::env::temp_dir().join(format!("axe-test-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        assert_eq!(
            paths_in("@", 1, &dir, None),
            vec!["link/".to_string(), "real/".to_string()],
            "a link to a directory is one"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_list_is_capped() {
        let dir = tree("cap");
        for i in 0..150 {
            fs::write(dir.join(format!("file{i:03}")), "").unwrap();
        }
        let found = paths_in("@file", 5, &dir, None);
        assert_eq!(found.len(), LIMIT);
        let _ = fs::remove_dir_all(&dir);
    }
}
