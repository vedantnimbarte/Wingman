//! `find_symbol`: locate the *definition* of a function/struct/trait/etc
//! by name. Much cheaper than a regex `grep` because it parses each file
//! once with tree-sitter and asks for the named declaration.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct FindSymbol;

#[derive(Debug, Deserialize)]
struct Args {
    /// Symbol name to search for. Case-sensitive exact match by default.
    name: String,
    /// Glob to restrict the search (defaults to all source files).
    #[serde(default)]
    glob: Option<String>,
    /// Maximum results to return.
    #[serde(default)]
    limit: Option<u32>,
    /// Allow case-insensitive matches.
    #[serde(default)]
    case_insensitive: bool,
}

#[async_trait]
impl Tool for FindSymbol {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find_symbol".into(),
            description: "Find where a function, struct, trait, class, or other symbol is *defined* \
                          (not just mentioned). Uses tree-sitter to parse files in supported languages \
                          (rust, python, javascript, typescript, tsx, go). Returns `path:line  kind  name  signature` \
                          rows."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "glob": { "type": "string", "description": "Optional glob to restrict the search (e.g. \"crates/**/*.rs\")." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 },
                    "case_insensitive": { "type": "boolean", "default": false }
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
        let needle_lower = needle.to_lowercase();

        let matcher = match args.glob.as_deref() {
            Some(g) => match globset::Glob::new(g) {
                Ok(gl) => Some(gl.compile_matcher()),
                Err(e) => return ToolOutcome::err(format!("bad glob: {e}")),
            },
            None => None,
        };

        let root = ctx.project_root.clone();
        let case_insensitive = args.case_insensitive;
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
                    if let Ok(rel) = path.strip_prefix(&root) {
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
                        let symbols = wingman_ts::extract_symbols(lang, &text);
                        for sym in symbols {
                            let matched = if case_insensitive {
                                sym.name.to_lowercase() == needle_lower
                            } else {
                                sym.name == needle
                            };
                            if matched {
                                out.push(format!(
                                    "{}:{}  {}  {}  {}",
                                    rel_str,
                                    sym.start_line,
                                    sym.kind.label(),
                                    sym.name,
                                    sym.signature,
                                ));
                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                    }
                }
                out
            }
            #[cfg(not(feature = "treesitter"))]
            {
                let _ = (root, needle, needle_lower, matcher, case_insensitive, limit);
                Vec::new()
            }
        })
        .await
        .unwrap_or_default();

        if hits.is_empty() {
            return ToolOutcome::ok(format!(
                "(no definition of `{}` found — try `grep_tool` for references, or check spelling)",
                args.name
            ));
        }
        ToolOutcome::ok(hits.join("\n"))
    }
}
