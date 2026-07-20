//! `who_calls`: find *references* to a symbol (call sites, mentions) across
//! the tree, each annotated with the enclosing function/method it appears in.
//!
//! This is the reference-side complement to `find_symbol` (which locates the
//! *definition*). The value over a plain `grep` is the enclosing-symbol
//! annotation: you learn *which* function calls the target, not just the raw
//! line. Answers "who uses this?" in one shot instead of 3–5 grep→read turns.
//!
//! ponytail: whole-word name-match heuristic, not resolved references. It can
//! over-report (same name, different symbol) and can't see dynamic/aliased
//! calls. Upgrade path: per-grammar reference queries in `wingman-ts`.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct WhoCalls;

#[derive(Debug, Deserialize)]
struct Args {
    /// Symbol name to find references to. Case-sensitive whole-word match.
    name: String,
    /// Glob to restrict the search (defaults to all source files).
    #[serde(default)]
    glob: Option<String>,
    /// Maximum results to return.
    #[serde(default)]
    limit: Option<u32>,
}

/// True if `line` contains `needle` as a whole identifier token (not a
/// substring of a longer identifier). Cheap replacement for a real lexer.
fn contains_word(line: &str, needle: &str) -> bool {
    let bytes = line.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() {
        return false;
    }
    let is_ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut i = 0;
    while let Some(pos) = line[i..].find(needle) {
        let start = i + pos;
        let end = start + nb.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
    }
    false
}

#[async_trait]
impl Tool for WhoCalls {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "who_calls".into(),
            description: "Find *references* to a function/struct/method by name (call sites, mentions), \
                          each annotated with the enclosing symbol it appears in. Unlike `grep`, tells you \
                          *which* function contains each reference. Skips the definition line itself. \
                          Whole-word match; supported languages: rust, python, javascript, typescript, tsx, go. \
                          Returns `path:line  [in enclosing]  <source line>` rows."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact symbol name (case-sensitive, whole word)." },
                    "glob": { "type": "string", "description": "Optional glob to restrict the search (e.g. \"crates/**/*.rs\")." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let limit = args.limit.unwrap_or(50).clamp(1, 200) as usize;
        let needle = args.name.clone();

        let matcher = match args.glob.as_deref() {
            Some(g) => match globset::Glob::new(g) {
                Ok(gl) => Some(gl.compile_matcher()),
                Err(e) => return ToolOutcome::err(format!("bad glob: {e}")),
            },
            None => None,
        };

        let root = ctx.project_root.clone();
        let hits = tokio::task::spawn_blocking(move || -> Vec<String> {
            #[cfg(feature = "treesitter")]
            {
                let mut out: Vec<String> = Vec::new();
                let walker = ignore::WalkBuilder::new(&root).build();
                for entry in walker.flatten() {
                    if out.len() >= limit {
                        break;
                    }
                    if entry.file_type().is_some_and(|t| t.is_dir()) {
                        continue;
                    }
                    let path = entry.path();
                    let Some(lang) = wingman_ts::Language::from_path(path) else {
                        continue;
                    };
                    let Ok(rel) = path.strip_prefix(&root) else {
                        continue;
                    };
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if let Some(m) = &matcher {
                        if !m.is_match(&rel_str) {
                            continue;
                        }
                    }
                    let Ok(bytes) = std::fs::read(path) else {
                        continue;
                    };
                    if bytes.iter().take(8192).any(|&b| b == 0) {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&bytes);

                    // Lines that are the *definition* of `needle` — skip them so
                    // who_calls reports only references, not the declaration.
                    let def_lines: std::collections::HashSet<u32> =
                        wingman_ts::extract_symbols(lang, &text)
                            .into_iter()
                            .filter(|s| s.name == needle)
                            .map(|s| s.start_line)
                            .collect();

                    for (idx, line) in text.lines().enumerate() {
                        let lineno = idx as u32 + 1;
                        if def_lines.contains(&lineno) || !contains_word(line, &needle) {
                            continue;
                        }
                        let enclosing = wingman_ts::enclosing_symbol(lang, &text, lineno)
                            .filter(|s| s.name != needle)
                            .map(|s| format!("  [in {} {}]", s.kind.label(), s.name))
                            .unwrap_or_default();
                        out.push(format!(
                            "{}:{}{}  {}",
                            rel_str,
                            lineno,
                            enclosing,
                            line.trim(),
                        ));
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
                out
            }
            #[cfg(not(feature = "treesitter"))]
            {
                let _ = (root, needle, matcher, limit);
                Vec::new()
            }
        })
        .await
        .unwrap_or_default();

        if hits.is_empty() {
            return ToolOutcome::ok(format!(
                "(no references to `{}` found — check spelling, or it may be defined but unused)",
                args.name
            ));
        }
        ToolOutcome::ok(hits.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::contains_word;

    #[test]
    fn whole_word_matching() {
        assert!(contains_word("    foo();", "foo"));
        assert!(contains_word("let x = foo(bar);", "foo"));
        assert!(!contains_word("let x = foobar();", "foo")); // substring, not word
        assert!(!contains_word("let foo_bar = 1;", "foo")); // ident continues
        assert!(contains_word("a.foo", "foo")); // dot is a boundary
        assert!(!contains_word("", "foo"));
        assert!(!contains_word("nothing here", "foo"));
    }
}
