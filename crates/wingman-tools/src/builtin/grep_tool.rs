//! `grep`: search for a regex pattern across files, respecting `.gitignore`.

use crate::{Tool, ToolCtx};
use wingman_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use globset::{Glob as GlobPat, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct Grep;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search for a regex pattern across files (gitignore-aware). Returns \
                          `path:line:content` lines. Default limit 200 matches."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Search root; defaults to project root." },
                    "include": { "type": "string", "description": "Glob filter on filenames (e.g. `*.rs`)." },
                    "case_insensitive": { "type": "boolean", "default": false },
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
        let mut pattern = args.pattern.clone();
        if args.case_insensitive {
            pattern = format!("(?i){pattern}");
        }
        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(e) => return ToolOutcome::err(format!("bad regex: {e}")),
        };
        let base = args
            .path
            .as_deref()
            .map(|p| ctx.resolve(p))
            .unwrap_or_else(|| ctx.project_root.clone());
        // Confine to the readable tree just like `read_file` — grep emits
        // matching file *content*, so an unchecked `path` outside the project
        // (e.g. `~/.aws`) would leak secrets in read-only/plan mode.
        if !ctx.allows_read(&base) {
            return ToolOutcome::err(format!(
                "read denied: {} is outside the project tree",
                base.display()
            ));
        }
        let include = match args.include.as_deref() {
            None => None,
            Some(s) => match GlobPat::new(s) {
                Ok(g) => match GlobSetBuilder::new().add(g).build() {
                    Ok(set) => Some(set),
                    Err(e) => return ToolOutcome::err(format!("bad include glob: {e}")),
                },
                Err(e) => return ToolOutcome::err(format!("bad include glob: {e}")),
            },
        };
        let limit = args.limit.unwrap_or(200) as usize;

        let base_for_task = base.clone();
        let matches: Vec<String> = tokio::task::spawn_blocking(move || {
            let mut out: Vec<String> = Vec::new();
            let walker = WalkBuilder::new(&base_for_task).build();
            'walk: for entry in walker.flatten() {
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    continue;
                }
                let path = entry.path();
                if let Some(set) = &include {
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy())
                        .unwrap_or_default();
                    if !set.is_match(name.as_ref()) {
                        continue;
                    }
                }
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                if bytes.iter().take(8192).any(|&b| b == 0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let rel = path.strip_prefix(&base_for_task).unwrap_or(path);
                        out.push(format!(
                            "{}:{}:{}",
                            rel.to_string_lossy().replace('\\', "/"),
                            i + 1,
                            line
                        ));
                        if out.len() >= limit {
                            break 'walk;
                        }
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
