//! `glob`: list files matching a glob pattern, respecting `.gitignore`.

use crate::{Tool, ToolCtx};
use wingman_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use globset::{Glob as GlobPat, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct Glob;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files matching a glob pattern (e.g. `**/*.rs`). Respects \
                          `.gitignore`. Returns relative paths. Limit defaults to 200."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "base": { "type": "string", "description": "Starting directory; defaults to project root." },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let base = args
            .base
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.project_root.clone());
        // Confine enumeration to the readable tree, matching `read_file`.
        if !ctx.allows_read(&base) {
            return ToolOutcome::err(format!(
                "read denied: {} is outside the project tree",
                base.display()
            ));
        }

        let pat = match GlobPat::new(&args.pattern) {
            Ok(g) => g,
            Err(e) => return ToolOutcome::err(format!("bad pattern: {e}")),
        };
        let set = match GlobSetBuilder::new().add(pat).build() {
            Ok(set) => set,
            Err(e) => return ToolOutcome::err(format!("bad pattern: {e}")),
        };
        let limit = args.limit.unwrap_or(200) as usize;
        let base_for_task = base.clone();

        let matches: Vec<String> = tokio::task::spawn_blocking(move || {
            let walker = WalkBuilder::new(&base_for_task).build();
            let mut out = Vec::new();
            for entry in walker.flatten() {
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&base_for_task)
                    .unwrap_or(entry.path());
                let s = rel.to_string_lossy();
                let normalized = s.replace('\\', "/");
                if set.is_match(&normalized) {
                    out.push(normalized);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default();

        if matches.is_empty() {
            ToolOutcome::ok("(no matches)")
        } else {
            ToolOutcome::ok(matches.join("\n"))
        }
    }
}
