use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct WriteFile;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Write a UTF-8 text file. Overwrites if it exists. Creates parent \
                          directories as needed. Requires write permission for the target."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
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
                "write denied for {} under permission mode {}",
                path.display(),
                ctx.mode()
            ));
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutcome::err(format!("mkdir {}: {e}", parent.display()));
            }
        }
        if let Err(e) = tokio::fs::write(&path, &args.content).await {
            return ToolOutcome::err(format!("write {}: {e}", path.display()));
        }
        ToolOutcome::ok(format!(
            "wrote {} ({} bytes)",
            path.display(),
            args.content.len()
        ))
    }
}
