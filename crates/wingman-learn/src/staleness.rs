//! Memory staleness: a memory that names project files which no longer exist
//! is probably rotting. We surface those so quality compounds instead of
//! decaying. File-path references only for now.
//!
//! ponytail: path-existence heuristic, not symbol resolution. Upgrade path is
//! checking named symbols against the tree-sitter/index symbol set.

use std::path::Path;

use crate::memory::Memory;

/// Pull project-relative file paths out of a memory body. A "path" here is a
/// whitespace/delimiter-separated token that contains a `/` and ends in a
/// short file extension (e.g. `crates/foo/src/lib.rs`, `docs/PLAN.md`).
/// Absolute paths, URLs, and home-relative paths are skipped — we can only
/// verify things under the project root.
pub fn referenced_paths(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in body.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | '<' | '>'
            )
    }) {
        let tok = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?'));
        if !looks_like_project_path(tok) {
            continue;
        }
        let tok = tok.to_string();
        if !out.contains(&tok) {
            out.push(tok);
        }
    }
    out
}

fn looks_like_project_path(tok: &str) -> bool {
    if tok.len() < 3 || !tok.contains('/') {
        return false;
    }
    // Skip absolute, home, and URL-ish references we can't resolve.
    if tok.starts_with('/') || tok.starts_with('~') || tok.contains("://") {
        return false;
    }
    // Require a short trailing extension: `.rs`, `.toml`, `.md`, …
    let last = tok.rsplit('/').next().unwrap_or(tok);
    match last.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=6).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Referenced project paths that no longer exist on disk. Empty ⇒ not stale.
pub fn missing_paths(memory: &Memory, project_root: &Path) -> Vec<String> {
    referenced_paths(&memory.body)
        .into_iter()
        .filter(|p| !project_root.join(p).exists())
        .collect()
}

/// All memories that reference at least one missing project file, paired with
/// the missing paths. `project_root` is the tree to resolve paths against.
pub fn stale_memories<'a>(
    memories: &'a [Memory],
    project_root: &Path,
) -> Vec<(&'a Memory, Vec<String>)> {
    memories
        .iter()
        .filter_map(|m| {
            let missing = missing_paths(m, project_root);
            (!missing.is_empty()).then_some((m, missing))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryScope, MemoryType};
    use std::path::PathBuf;

    fn mem(body: &str) -> Memory {
        Memory {
            name: "m".into(),
            description: "d".into(),
            mtype: MemoryType::Project,
            body: body.into(),
            scope: MemoryScope::Project,
            path: PathBuf::from("x.md"),
        }
    }

    #[test]
    fn extracts_only_pathlike_tokens() {
        let paths = referenced_paths(
            "See `crates/foo/src/lib.rs` and docs/PLAN.md, but not AgentLoop::run \
             or https://x.dev/a.rs or /etc/passwd or ~/.wingman/config.toml.",
        );
        assert!(paths.contains(&"crates/foo/src/lib.rs".to_string()));
        assert!(paths.contains(&"docs/PLAN.md".to_string()));
        assert!(!paths.iter().any(|p| p.contains("passwd")));
        assert!(!paths.iter().any(|p| p.contains("://")));
        assert!(!paths.iter().any(|p| p.starts_with('~')));
        assert!(!paths.iter().any(|p| p.contains("AgentLoop")));
    }

    #[test]
    fn flags_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/here.rs"), "").unwrap();
        let m = mem("real: src/here.rs, gone: src/gone.rs");
        let missing = missing_paths(&m, dir.path());
        assert_eq!(missing, vec!["src/gone.rs".to_string()]);

        let stale = stale_memories(std::slice::from_ref(&m), dir.path());
        assert_eq!(stale.len(), 1);
    }
}
