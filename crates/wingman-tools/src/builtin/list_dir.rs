use crate::{Tool, ToolCtx};
use wingman_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ListDir;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    hidden: bool,
}

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "List entries in a directory. Each line is `D <name>` or `F <name>`. \
                          Hidden entries are skipped unless `hidden: true`."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "hidden": { "type": "boolean", "default": false }
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
        // Confine enumeration to the readable tree, matching `read_file`.
        if !ctx.allows_read(&path) {
            return ToolOutcome::err(format!(
                "read denied: {} is outside the project tree",
                path.display()
            ));
        }
        let mut rd = match tokio::fs::read_dir(&path).await {
            Ok(rd) => rd,
            Err(e) => return ToolOutcome::err(format!("readdir {}: {e}", path.display())),
        };
        let mut entries: Vec<(bool, String)> = Vec::new();
        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !args.hidden && name.starts_with('.') {
                        continue;
                    }
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    entries.push((is_dir, name));
                }
                Ok(None) => break,
                Err(e) => return ToolOutcome::err(format!("iter: {e}")),
            }
        }
        entries.sort_by(|a, b| (!a.0, &a.1).cmp(&(!b.0, &b.1)));
        let out: String = entries
            .into_iter()
            .map(|(is_dir, name)| format!("{} {}", if is_dir { "D" } else { "F" }, name))
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutcome::ok(out)
    }
}
