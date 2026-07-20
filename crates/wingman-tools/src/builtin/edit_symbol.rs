//! `edit_symbol`: AST-aware function/method body replacement.
//!
//! Where `edit_file` does exact string replacement (and can match the wrong
//! copy of a string), this tool finds the named function via tree-sitter
//! and replaces just its body. The outer signature and surrounding code are
//! preserved. Returns a unified diff so the model can verify the edit.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct EditSymbol;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    /// Function or method name. Method dispatch picks the first match in
    /// source order — disambiguate with `read_file` first when needed.
    name: String,
    /// Replacement body. For brace-delimited languages, supply the body
    /// *without* the outer `{` and `}`. For Python, supply the indented
    /// block body.
    new_body: String,
}

#[async_trait]
impl Tool for EditSymbol {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_symbol".into(),
            description: "Replace the body of a named function or method using tree-sitter (rust, python, \
                          javascript, typescript, tsx, go). Safer than `edit_file` when the same text \
                          appears in multiple places. `new_body` excludes outer braces for brace-delimited \
                          languages; supply the indented block for Python. Returns a unified diff."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "name": { "type": "string", "description": "Function/method name to replace." },
                    "new_body": { "type": "string", "description": "Replacement body (no outer braces)." }
                },
                "required": ["path", "name", "new_body"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let path = ctx.resolve(&args.path);
        if !ctx.allows_write(&path) {
            return ToolOutcome::err(format!(
                "edit denied for {} under permission mode {}",
                path.display(),
                ctx.mode()
            ));
        }
        #[cfg(feature = "treesitter")]
        {
            let Some(lang) = wingman_ts::Language::from_path(&path) else {
                return ToolOutcome::err(format!(
                    "edit_symbol does not support {}: unknown language",
                    path.display()
                ));
            };
            let original = match tokio::fs::read_to_string(&path).await {
                Ok(s) => s,
                Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
            };
            let updated = match wingman_ts::replace_function_body(
                lang,
                &original,
                &args.name,
                &args.new_body,
            ) {
                Some(s) => s,
                None => {
                    return ToolOutcome::err(format!(
                        "no function or method named `{}` found in {}",
                        args.name,
                        path.display()
                    ))
                }
            };
            if updated == original {
                return ToolOutcome::err("no change after replacement");
            }
            if let Err(e) = tokio::fs::write(&path, &updated).await {
                return ToolOutcome::err(format!("write {}: {e}", path.display()));
            }
            ToolOutcome::ok(unified_diff(&original, &updated, &args.path))
        }
        #[cfg(not(feature = "treesitter"))]
        {
            let _ = path;
            ToolOutcome::err("edit_symbol requires the treesitter feature")
        }
    }
}

fn unified_diff(old: &str, new: &str, label: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- a/{label}\n+++ b/{label}\n"));
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            out.push_str(sign);
            out.push_str(change.value());
            if !change.value().ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(all(test, feature = "treesitter"))]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    #[tokio::test]
    async fn replaces_rust_function_body() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-edit-symbol-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn add(a: u32, b: u32) -> u32 { a + b }\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.clone(), dir.clone());
        let out = EditSymbol
            .run(
                json!({
                    "path": path.to_string_lossy(),
                    "name": "add",
                    "new_body": " a.saturating_add(b) "
                }),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "got error: {}", out.content);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("a.saturating_add(b)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
