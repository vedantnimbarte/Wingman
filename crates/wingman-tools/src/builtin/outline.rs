//! `outline`: signatures-only view of a source file.
//!
//! Returns one line per declaration, indented under containers. Useful when
//! you want to know what's in a file without burning the token budget on
//! its bodies.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct Outline;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
}

#[async_trait]
impl Tool for Outline {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "outline".into(),
            description: "Return a signatures-only outline of a source file (one line per fn/struct/class/etc) \
                          so you can see its shape without reading every body. Supported languages: rust, python, \
                          javascript, typescript, tsx, go. Falls back to a short message for unsupported types."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute or cwd-relative path." }
                },
                "required": ["path"],
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
        #[cfg(feature = "treesitter")]
        {
            let Some(lang) = wingman_ts::Language::from_path(&path) else {
                return ToolOutcome::err(format!(
                    "unsupported language for outline: {}",
                    path.display()
                ));
            };
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
            };
            if bytes.iter().take(8192).any(|&b| b == 0) {
                return ToolOutcome::err(format!("refusing binary file {}", path.display()));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            match wingman_ts::outline(lang, &text) {
                Some(out) if !out.is_empty() => ToolOutcome::ok(out.trim_end().to_string()),
                _ => ToolOutcome::ok(format!(
                    "(no top-level declarations found in {})",
                    path.display()
                )),
            }
        }
        #[cfg(not(feature = "treesitter"))]
        {
            let _ = path;
            ToolOutcome::err("outline requires the treesitter feature")
        }
    }
}
