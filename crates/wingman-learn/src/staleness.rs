//! Memory staleness: a memory that names project files or code symbols which
//! no longer exist is probably rotting. We surface those so quality compounds
//! instead of decaying.
//!
//! Paths are checked on disk. Symbols are checked by a resolver the caller
//! injects (`wingman knows` backs it with the tree-sitter index and the
//! language server's `workspace/symbol`), so this crate stays free of any
//! language-server dependency; with no resolver only paths are checked.
//!
//! ponytail: a symbol is resolved by the first type name in its path, so a
//! deleted method on a live type (`AgentLoop::gone`) does not read as stale.
//! Upgrade path is resolving the qualified path with the language server.

use std::path::Path;

use crate::memory::{Memory, MemoryScope};

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

/// Names of the code symbols a memory refers to in backticks. Only shapes that
/// read as a project symbol count, since a bare backticked word is as often a
/// command or a config key: a type (`AgentLoop`), a path through one
/// (`AgentLoop::run` resolves `AgentLoop`), or a call (`stale_memories()`). A
/// module path with no type in it (`std::fs::read`) is skipped.
pub fn referenced_symbols(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Odd pieces of a split on backticks are the code spans.
    for span in body.split('`').skip(1).step_by(2) {
        let (path, call) = match span.strip_suffix("()") {
            Some(p) => (p, true),
            None => (span, false),
        };
        let segments: Vec<&str> = path.split("::").collect();
        if !segments.iter().all(|s| is_identifier(s)) {
            continue;
        }
        let name = match segments.iter().find(|s| is_type_name(s)) {
            Some(ty) => *ty,
            None if call && segments.len() == 1 => segments[0],
            None => continue,
        };
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `AgentLoop`, `Config` — not `README` or `HTTP`, which are rarely types.
fn is_type_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase()) && s.chars().any(|c| c.is_ascii_lowercase())
}

/// Referenced project paths that no longer exist on disk. Empty ⇒ not stale.
pub fn missing_paths(memory: &Memory, project_root: &Path) -> Vec<String> {
    referenced_paths(&memory.body)
        .into_iter()
        .filter(|p| !project_root.join(p).exists())
        .collect()
}

/// All memories that reference at least one missing project file or, when
/// `symbol_exists` is given, a symbol it says the project no longer has —
/// paired with what's missing (paths as written, symbols in backticks).
/// `project_root` is the tree to resolve paths against. Symbols are checked
/// for project memories only: a global one names code from whichever project
/// it was learned in.
pub fn stale_memories<'a>(
    memories: &'a [Memory],
    project_root: &Path,
    symbol_exists: Option<&dyn Fn(&str) -> bool>,
) -> Vec<(&'a Memory, Vec<String>)> {
    memories
        .iter()
        .filter_map(|m| {
            let mut missing = missing_paths(m, project_root);
            if let (Some(exists), MemoryScope::Project) = (symbol_exists, m.scope) {
                missing.extend(
                    referenced_symbols(&m.body)
                        .into_iter()
                        .filter(|s| !exists(s))
                        .map(|s| format!("`{s}`")),
                );
            }
            (!missing.is_empty()).then_some((m, missing))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryType;
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

        let stale = stale_memories(std::slice::from_ref(&m), dir.path(), None);
        assert_eq!(stale.len(), 1);
    }

    #[test]
    fn extracts_only_symbol_shaped_code_spans() {
        let names = referenced_symbols(
            "`AgentLoop::run` drives `LspClient`; call `stale_memories()` then \
             `wingman_lsp::Lang::Rust`. Not `cargo test`, `turn_gate`, `README`, \
             `std::fs::read`, `docs/PLAN.md`, or `AgentLoop` twice.",
        );
        assert_eq!(
            names,
            vec!["AgentLoop", "LspClient", "stale_memories", "Lang"]
        );
    }

    #[test]
    fn flags_symbols_the_resolver_cannot_find() {
        let dir = tempfile::tempdir().unwrap();
        let live = mem("`AgentLoop` is the loop");
        let gone = mem("`OldRouter::route` picks the model, see src/gone.rs");
        let memories = [live, gone];
        let exists = |s: &str| s == "AgentLoop";

        let stale = stale_memories(&memories, dir.path(), Some(&exists));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].1, vec!["src/gone.rs", "`OldRouter`"]);

        // Without a resolver only the path counts.
        let stale = stale_memories(&memories, dir.path(), None);
        assert_eq!(stale[0].1, vec!["src/gone.rs"]);

        // A global memory's symbols belong to some other project.
        let mut global = mem("`OldRouter` picks the model");
        global.scope = MemoryScope::Global;
        assert!(stale_memories(&[global], dir.path(), Some(&exists)).is_empty());
    }
}
