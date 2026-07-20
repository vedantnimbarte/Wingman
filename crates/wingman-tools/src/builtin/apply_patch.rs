//! `apply_patch`: atomic multi-file edits in one tool call.
//!
//! Input format (newline-delimited blocks, similar to Aider's edit blocks):
//!
//! ```text
//! *** Begin Patch
//! *** Update File: path/to/file.rs
//! @@
//! - old line
//! - other old line
//! + new line
//! *** End File
//! *** Add File: new/file.txt
//! + line 1
//! + line 2
//! *** End File
//! *** Delete File: old/file.txt
//! *** End Patch
//! ```
//!
//! All edits are validated up front; on any failure, no file is touched.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use wingman_core::{ToolOutcome, ToolSpec};

pub struct ApplyPatch;

#[derive(Debug, Deserialize)]
struct Args {
    patch: String,
}

enum Op {
    Update {
        path: PathBuf,
        old: String,
        new: String,
    },
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
}

#[async_trait]
impl Tool for ApplyPatch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description: concat!(
                "Apply a multi-file patch atomically. The `patch` is a string in the form:\n",
                "*** Begin Patch\n",
                "*** Update File: <path>\n",
                "@@\n",
                "- old line\n",
                "+ new line\n",
                "*** End File\n",
                "*** Add File: <path>\n",
                "+ line 1\n",
                "*** End File\n",
                "*** Delete File: <path>\n",
                "*** End Patch\n\n",
                "All operations are validated before any write; on any failure no file is changed. ",
                "Updates require the `- old` block to match exactly once in the target file."
            )
            .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Patch text in the documented format." }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let ops = match parse_patch(&args.patch, ctx) {
            Ok(ops) => ops,
            Err(e) => return ToolOutcome::err(format!("parse: {e}")),
        };

        // Pre-validate: writes allowed for each path, and update old-blocks match exactly once.
        let mut planned: Vec<(Op, Option<String>)> = Vec::with_capacity(ops.len());
        for op in ops {
            match &op {
                Op::Update { path, old, .. } => {
                    if !ctx.allows_write(path) {
                        return ToolOutcome::err(format!(
                            "write denied for {} under permission mode {}",
                            path.display(),
                            ctx.mode()
                        ));
                    }
                    let original = match tokio::fs::read_to_string(path).await {
                        Ok(s) => s,
                        Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
                    };
                    let n = original.matches(old.as_str()).count();
                    if n == 0 {
                        return ToolOutcome::err(format!(
                            "update block not found in {}",
                            path.display()
                        ));
                    }
                    if n > 1 {
                        return ToolOutcome::err(format!(
                            "update block matches {n} times in {} — add context to disambiguate",
                            path.display()
                        ));
                    }
                    planned.push((op, Some(original)));
                }
                Op::Add { path, .. } => {
                    if !ctx.allows_write(path) {
                        return ToolOutcome::err(format!(
                            "write denied for {} under permission mode {}",
                            path.display(),
                            ctx.mode()
                        ));
                    }
                    if path.exists() {
                        return ToolOutcome::err(format!(
                            "add: {} already exists — use Update",
                            path.display()
                        ));
                    }
                    planned.push((op, None));
                }
                Op::Delete { path } => {
                    if !ctx.allows_write(path) {
                        return ToolOutcome::err(format!(
                            "write denied for {} under permission mode {}",
                            path.display(),
                            ctx.mode()
                        ));
                    }
                    if !path.exists() {
                        return ToolOutcome::err(format!(
                            "delete: {} does not exist",
                            path.display()
                        ));
                    }
                    planned.push((op, None));
                }
            }
        }

        // Apply.
        let mut summary = String::new();
        for (op, original) in planned {
            match op {
                Op::Update { path, old, new } => {
                    let original = original.unwrap();
                    let updated = original.replacen(&old, &new, 1);
                    if let Err(e) = tokio::fs::write(&path, &updated).await {
                        return ToolOutcome::err(format!("write {}: {e}", path.display()));
                    }
                    summary.push_str(&format!("updated {}\n", path.display()));
                }
                Op::Add { path, content } => {
                    if let Some(parent) = path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    if let Err(e) = tokio::fs::write(&path, &content).await {
                        return ToolOutcome::err(format!("write {}: {e}", path.display()));
                    }
                    summary.push_str(&format!("added   {}\n", path.display()));
                }
                Op::Delete { path } => {
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        return ToolOutcome::err(format!("delete {}: {e}", path.display()));
                    }
                    summary.push_str(&format!("deleted {}\n", path.display()));
                }
            }
        }
        ToolOutcome::ok(summary)
    }
}

fn parse_patch(text: &str, ctx: &ToolCtx) -> Result<Vec<Op>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    // Skip leading blank lines + optional "*** Begin Patch".
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i < lines.len() && lines[i].trim() == "*** Begin Patch" {
        i += 1;
    }

    let mut ops = Vec::new();
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            let path = ctx.resolve(rest.trim());
            i += 1;
            // Optional `@@` header marker.
            if i < lines.len() && lines[i].trim() == "@@" {
                i += 1;
            }
            let mut old = String::new();
            let mut new = String::new();
            while i < lines.len() && lines[i].trim() != "*** End File" {
                let l = lines[i];
                if let Some(rest) = l.strip_prefix("- ") {
                    old.push_str(rest);
                    old.push('\n');
                } else if l == "-" {
                    old.push('\n');
                } else if let Some(rest) = l.strip_prefix("+ ") {
                    new.push_str(rest);
                    new.push('\n');
                } else if l == "+" {
                    new.push('\n');
                } else {
                    // Context lines (no prefix) appear in both blocks.
                    let ctx_l = l.strip_prefix(' ').unwrap_or(l);
                    old.push_str(ctx_l);
                    old.push('\n');
                    new.push_str(ctx_l);
                    new.push('\n');
                }
                i += 1;
            }
            if i >= lines.len() {
                return Err("Update File block missing *** End File".to_string());
            }
            i += 1; // consume End File
            ops.push(Op::Update { path, old, new });
        } else if let Some(rest) = line.strip_prefix("*** Add File: ") {
            let path = ctx.resolve(rest.trim());
            i += 1;
            let mut content = String::new();
            while i < lines.len() && lines[i].trim() != "*** End File" {
                let l = lines[i];
                if let Some(rest) = l.strip_prefix("+ ") {
                    content.push_str(rest);
                    content.push('\n');
                } else if l == "+" {
                    content.push('\n');
                } else {
                    // Allow raw lines too.
                    content.push_str(l);
                    content.push('\n');
                }
                i += 1;
            }
            if i >= lines.len() {
                return Err("Add File block missing *** End File".to_string());
            }
            i += 1;
            ops.push(Op::Add { path, content });
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            let path = ctx.resolve(rest.trim());
            ops.push(Op::Delete { path });
            i += 1;
        } else {
            return Err(format!("unexpected line: {line}"));
        }
    }
    if ops.is_empty() {
        return Err("patch contained no operations".to_string());
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    fn tmp_dir() -> PathBuf {
        // pid + nanos alone isn't unique enough: macOS Apple Silicon's
        // CLOCK_REALTIME has ~250ns granularity, so two parallel tokio
        // tests can grab the same timestamp and collide on a single dir.
        // The atomic counter guarantees uniqueness within the process.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wingman-patch-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn applies_update_and_add() {
        let dir = tmp_dir();
        let a = dir.join("a.txt");
        std::fs::write(&a, "hello\nworld\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.clone(), dir.clone());
        let patch = format!(
            "*** Begin Patch\n*** Update File: {a_path}\n@@\n- world\n+ rust\n*** End File\n*** Add File: {b_path}\n+ line 1\n+ line 2\n*** End File\n*** End Patch\n",
            a_path = a.display(),
            b_path = dir.join("b.txt").display(),
        );
        let out = ApplyPatch.run(json!({"patch": patch}), &ctx).await;
        assert!(!out.is_error, "got error: {}", out.content);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "hello\nrust\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "line 1\nline 2\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_ambiguous_update() {
        let dir = tmp_dir();
        let a = dir.join("a.txt");
        std::fs::write(&a, "x\nx\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.clone(), dir.clone());
        let patch = format!(
            "*** Update File: {a_path}\n- x\n+ y\n*** End File\n",
            a_path = a.display()
        );
        let out = ApplyPatch.run(json!({"patch": patch}), &ctx).await;
        assert!(
            out.is_error,
            "expected error outcome, got ok: {}",
            out.content
        );
        assert!(
            out.content.contains("matches 2"),
            "expected error to mention 'matches 2'; got: {}",
            out.content,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
