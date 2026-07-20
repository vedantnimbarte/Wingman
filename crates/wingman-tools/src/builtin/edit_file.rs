//! `edit_file`: exact string replacement with similar-validated diff output.
//!
//! Default behavior is one-shot replace: `old_string` must appear exactly
//! once or the call fails (forcing the model to disambiguate). Pass
//! `replace_all: true` to replace every occurrence. Returns a unified diff
//! of what changed so the model can verify its own edit.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct EditFile;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace `old_string` with `new_string` in a file. Fails if `old_string` \
                          is missing, or if it appears more than once unless `replace_all` is \
                          true. Returns a unified diff of the change."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Text to replace it with." },
                    "replace_all": {
                        "type": "boolean",
                        "default": false,
                        "description": "Replace every occurrence instead of requiring uniqueness."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        if args.old_string == args.new_string {
            return ToolOutcome::err("old_string and new_string are identical");
        }
        let path = ctx.resolve(&args.path);
        if !ctx.allows_write(&path) {
            return ToolOutcome::err(format!(
                "edit denied for {} under permission mode {}",
                path.display(),
                ctx.mode()
            ));
        }

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };

        let occurrences = original.matches(&args.old_string).count();
        if occurrences == 0 {
            return ToolOutcome::err(format!("old_string not found in {}", path.display()));
        }
        if occurrences > 1 && !args.replace_all {
            return ToolOutcome::err(format!(
                "old_string appears {} times in {} — pass replace_all or include more context",
                occurrences,
                path.display()
            ));
        }

        let updated = if args.replace_all {
            original.replace(&args.old_string, &args.new_string)
        } else {
            original.replacen(&args.old_string, &args.new_string, 1)
        };

        if updated == original {
            return ToolOutcome::err("no change after replacement");
        }

        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return ToolOutcome::err(format!("write {}: {e}", path.display()));
        }

        ToolOutcome::ok(unified_diff(&original, &updated, &args.path))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wingman_config::PermissionMode;

    fn tmp_dir() -> PathBuf {
        // pid + nanos can collide between parallel tokio tests on macOS
        // Apple Silicon (CLOCK_REALTIME has ~250ns granularity). Add an
        // atomic counter to guarantee uniqueness within the process.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wingman-edit-{}-{}-{n}",
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
    async fn edits_file_and_returns_diff() {
        let dir = tmp_dir();
        let path = dir.join("x.txt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.clone(), dir.clone());
        let out = EditFile
            .run(
                json!({
                    "path": path.to_string_lossy(),
                    "old_string": "world",
                    "new_string": "rust"
                }),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "got error: {}", out.content);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "hello\nrust\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_ambiguous_edit() {
        let dir = tmp_dir();
        let path = dir.join("x.txt");
        std::fs::write(&path, "ab\nab\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::Yolo, dir.clone(), dir.clone());
        let out = EditFile
            .run(
                json!({"path": path.to_string_lossy(), "old_string": "ab", "new_string": "cd"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("2 times"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn denies_write_in_read_only_mode() {
        let dir = tmp_dir();
        let path = dir.join("x.txt");
        std::fs::write(&path, "a\n").unwrap();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let out = EditFile
            .run(
                json!({"path": path.to_string_lossy(), "old_string": "a", "new_string": "b"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("denied"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
