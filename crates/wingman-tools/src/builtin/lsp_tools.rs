//! LSP-backed code-intelligence tools: real, resolved go-to-definition,
//! find-references, hover, diagnostics, and rename — the semantic upgrade over
//! the tree-sitter heuristics (`find_symbol`, `who_calls`).
//!
//! Backed by whatever language server the user has on `PATH` (rust-analyzer,
//! pyright/pylsp, typescript-language-server, gopls). When no server is
//! installed the tools return a clear note telling the agent to fall back to
//! the heuristic tools, rather than erroring — graceful degradation.
//!
//! Ergonomics: positions can be given as an exact `(line, character)` (both
//! 1-based, as shown in editors/`grep` output) OR as a `line` plus a `symbol`
//! name we locate on that line. The latter is what models reliably produce.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_lsp::{client::Position, Diagnostic, Location};

#[derive(Debug, Deserialize)]
struct PosArgs {
    /// File path (relative to project root or absolute).
    path: String,
    /// 1-based line number (as shown in editors and grep output).
    line: u32,
    /// 1-based column. Optional if `symbol` is given.
    #[serde(default)]
    character: Option<u32>,
    /// Symbol name to locate on `line` to derive the column. Preferred over
    /// `character` — models produce names reliably, exact columns rarely.
    #[serde(default)]
    symbol: Option<String>,
}

/// Resolve the tool's `(line, character-or-symbol)` into a 0-based LSP
/// [`Position`]. Reads the file to locate `symbol` when a column isn't given.
fn resolve_position(abs: &Path, a: &PosArgs) -> Result<Position, String> {
    let line0 = a.line.saturating_sub(1);
    // Explicit 1-based column wins when present.
    if let Some(col) = a.character {
        return Ok(Position {
            line: line0,
            character: col.saturating_sub(1),
        });
    }
    let symbol = a
        .symbol
        .as_deref()
        .ok_or("provide either `character` (1-based column) or `symbol`")?;
    let text =
        std::fs::read_to_string(abs).map_err(|e| format!("cannot read {}: {e}", abs.display()))?;
    let line_text = text
        .lines()
        .nth(line0 as usize)
        .ok_or_else(|| format!("line {} is past end of {}", a.line, abs.display()))?;
    let byte = line_text
        .find(symbol)
        .ok_or_else(|| format!("symbol `{symbol}` not found on line {}", a.line))?;
    // UTF-16 offset of `byte` within the line.
    let character = line_text[..byte].encode_utf16().count() as u32;
    Ok(Position {
        line: line0,
        character,
    })
}

/// Common preamble: resolve+read-gate the path and fetch the language client.
/// Returns `Err(ToolOutcome)` already-formed for the unavailable / denied cases.
async fn client_for(
    ctx: &ToolCtx,
    raw_path: &str,
) -> Result<(PathBuf, std::sync::Arc<wingman_lsp::LspClient>), ToolOutcome> {
    let abs = ctx.resolve(raw_path);
    if !ctx.allows_read(&abs) {
        return Err(ToolOutcome::err(format!(
            "reading {} is not permitted in the current mode",
            abs.display()
        )));
    }
    let manager = wingman_lsp::manager_for(&ctx.project_root).await;
    match manager.client_for_path(&abs).await {
        Ok(c) => Ok((abs, c)),
        Err(unavailable) => Err(ToolOutcome::ok(format!(
            "(LSP unavailable: {unavailable}. Fall back to `find_symbol` / `who_calls` \
             for a tree-sitter answer.)"
        ))),
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn format_locations(root: &Path, locs: &[Location]) -> String {
    if locs.is_empty() {
        return "(no results)".into();
    }
    locs.iter()
        // LSP is 0-based; report 1-based to match editors/grep.
        .map(|l| format!("{}:{}:{}", rel(root, &l.path), l.line + 1, l.character + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- lsp_definition -------------------------------------------------------

pub struct LspDefinition;

#[async_trait]
impl Tool for LspDefinition {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_definition".into(),
            description: "Resolve where a symbol is DEFINED using the language server (rust-analyzer, \
                          pyright, typescript-language-server, gopls). Real go-to-definition — resolves \
                          imports, types, and re-exports, unlike name-matching. Give `path` + `line` and \
                          either `symbol` (name on that line) or `character` (1-based column). Returns \
                          `path:line:col` locations."
                .into(),
            input_schema: pos_schema(),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: PosArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let (abs, client) = match client_for(ctx, &a.path).await {
            Ok(v) => v,
            Err(out) => return out,
        };
        let pos = match resolve_position(&abs, &a) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(e),
        };
        match client.definition(&abs, pos).await {
            Ok(locs) => ToolOutcome::ok(format_locations(&ctx.project_root, &locs)),
            Err(e) => ToolOutcome::err(format!("lsp definition failed: {e}")),
        }
    }
}

// ---- lsp_references -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RefArgs {
    #[serde(flatten)]
    pos: PosArgs,
    /// Include the declaration itself among the results (default false).
    #[serde(default)]
    include_declaration: bool,
}

pub struct LspReferences;

#[async_trait]
impl Tool for LspReferences {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_references".into(),
            description: "Find every RESOLVED reference to a symbol via the language server — true \
                          call/use sites across the project, not name matches. Prefer this over `who_calls` \
                          when a server is available (it won't over-report same-named-but-different symbols). \
                          Give `path` + `line` + (`symbol` or `character`). Returns `path:line:col` rows."
                .into(),
            input_schema: {
                let mut s = pos_schema();
                s["properties"]["include_declaration"] = json!({
                    "type": "boolean",
                    "description": "Include the declaration site in results (default false)."
                });
                s
            },
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: RefArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let (abs, client) = match client_for(ctx, &a.pos.path).await {
            Ok(v) => v,
            Err(out) => return out,
        };
        let pos = match resolve_position(&abs, &a.pos) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(e),
        };
        match client.references(&abs, pos, a.include_declaration).await {
            Ok(locs) => ToolOutcome::ok(format_locations(&ctx.project_root, &locs)),
            Err(e) => ToolOutcome::err(format!("lsp references failed: {e}")),
        }
    }
}

// ---- lsp_hover ------------------------------------------------------------

pub struct LspHover;

#[async_trait]
impl Tool for LspHover {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_hover".into(),
            description: "Get the type / signature / doc summary the language server shows on hover for a \
                          symbol — resolved types and inferred signatures you can't get from the source text \
                          alone. Give `path` + `line` + (`symbol` or `character`)."
                .into(),
            input_schema: pos_schema(),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: PosArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let (abs, client) = match client_for(ctx, &a.path).await {
            Ok(v) => v,
            Err(out) => return out,
        };
        let pos = match resolve_position(&abs, &a) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(e),
        };
        match client.hover(&abs, pos).await {
            Ok(Some(text)) => ToolOutcome::ok(text),
            Ok(None) => ToolOutcome::ok("(no hover information at that position)"),
            Err(e) => ToolOutcome::err(format!("lsp hover failed: {e}")),
        }
    }
}

// ---- lsp_diagnostics ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DiagArgs {
    /// File to collect diagnostics for.
    path: String,
    /// Only report errors (severity 1), skipping warnings/hints.
    #[serde(default)]
    errors_only: bool,
}

pub struct LspDiagnostics;

#[async_trait]
impl Tool for LspDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_diagnostics".into(),
            description: "Get the language server's live diagnostics (errors, warnings) for a file — the \
                          same red squiggles an editor shows, including type errors the compiler would raise. \
                          Use after editing to check your change is clean. Returns `line:col severity: message` \
                          rows, or a clean-file note."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path (relative or absolute)." },
                    "errors_only": { "type": "boolean", "description": "Report only errors, not warnings (default false)." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: DiagArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let (abs, client) = match client_for(ctx, &a.path).await {
            Ok(v) => v,
            Err(out) => return out,
        };
        // Servers publish diagnostics asynchronously after didOpen; give a
        // cold server (rust-analyzer indexing) time to answer.
        let diags = match client
            .diagnostics(&abs, std::time::Duration::from_secs(20))
            .await
        {
            Ok(d) => d,
            Err(e) => return ToolOutcome::err(format!("lsp diagnostics failed: {e}")),
        };
        ToolOutcome::ok(format_diagnostics(&diags, a.errors_only))
    }
}

fn format_diagnostics(diags: &[Diagnostic], errors_only: bool) -> String {
    let filtered: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| !errors_only || d.is_error())
        .collect();
    if filtered.is_empty() {
        return "(no diagnostics — file is clean)".into();
    }
    filtered
        .iter()
        .map(|d| {
            let src = d
                .source
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            format!(
                "{}:{} {}{}: {}",
                d.line + 1,
                d.character + 1,
                d.severity_label(),
                src,
                d.message.replace('\n', " ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- lsp_rename -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RenameArgs {
    #[serde(flatten)]
    pos: PosArgs,
    /// The new identifier.
    new_name: String,
}

pub struct LspRename;

#[async_trait]
impl Tool for LspRename {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp_rename".into(),
            description: "Rename a symbol project-wide via the language server, updating every resolved \
                          reference across files atomically and correctly (not text substitution). Give \
                          `path` + `line` + (`symbol` or `character`) + `new_name`. Requires write permission. \
                          Returns the list of files changed."
                .into(),
            input_schema: {
                let mut s = pos_schema();
                s["properties"]["new_name"] =
                    json!({ "type": "string", "description": "The new identifier." });
                s["required"] = json!(["path", "line", "new_name"]);
                s
            },
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let a: RenameArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let abs = ctx.resolve(&a.pos.path);
        // A rename writes across the project — gate on write permission.
        if !ctx.allows_write(&abs) {
            return ToolOutcome::err(
                "lsp_rename edits files; not permitted in the current mode (needs auto-edit/yolo)",
            );
        }
        let (abs, client) = match client_for(ctx, &a.pos.path).await {
            Ok(v) => v,
            Err(out) => return out,
        };
        let pos = match resolve_position(&abs, &a.pos) {
            Ok(p) => p,
            Err(e) => return ToolOutcome::err(e),
        };
        let edit = match client.rename(&abs, pos, &a.new_name).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                return ToolOutcome::err("the language server declined to rename at that position")
            }
            Err(e) => return ToolOutcome::err(format!("lsp rename failed: {e}")),
        };
        match wingman_lsp::edit::apply_workspace_edit(&edit).await {
            Ok(changed) if changed.is_empty() => ToolOutcome::ok("(rename produced no changes)"),
            Ok(changed) => {
                let list = changed
                    .iter()
                    .map(|p| rel(&ctx.project_root, p))
                    .collect::<Vec<_>>()
                    .join("\n");
                ToolOutcome::ok(format!(
                    "renamed to `{}` across {} file(s):\n{list}",
                    a.new_name,
                    changed.len()
                ))
            }
            Err(e) => ToolOutcome::err(format!("applying rename edit failed: {e}")),
        }
    }
}

/// The shared JSON schema for position-taking tools.
fn pos_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File path (relative to project root or absolute)." },
            "line": { "type": "integer", "minimum": 1, "description": "1-based line number." },
            "character": { "type": "integer", "minimum": 1, "description": "1-based column. Optional if `symbol` is given." },
            "symbol": { "type": "string", "description": "Symbol name to locate on `line` (preferred over `character`)." }
        },
        "required": ["path", "line"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_position_uses_explicit_column() {
        let args = PosArgs {
            path: "x".into(),
            line: 3,
            character: Some(5),
            symbol: None,
        };
        let p = resolve_position(Path::new("nonexistent"), &args).unwrap();
        assert_eq!(p.line, 2); // 0-based
        assert_eq!(p.character, 4);
    }

    #[test]
    fn resolve_position_locates_symbol_in_file() {
        let dir = std::env::temp_dir().join(format!("wm-lsp-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.rs");
        std::fs::write(&file, "fn a() {}\nlet value = compute();\n").unwrap();
        let args = PosArgs {
            path: file.to_string_lossy().into(),
            line: 2,
            character: None,
            symbol: Some("compute".into()),
        };
        let p = resolve_position(&file, &args).unwrap();
        assert_eq!(p.line, 1);
        // "let value = " is 12 chars → compute starts at col 12 (0-based).
        assert_eq!(p.character, 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_diagnostics_reports_clean() {
        assert!(format_diagnostics(&[], false).contains("clean"));
    }

    #[test]
    fn format_diagnostics_filters_errors_only() {
        let diags = vec![
            Diagnostic {
                line: 0,
                character: 0,
                severity: 2,
                message: "warn".into(),
                source: None,
            },
            Diagnostic {
                line: 4,
                character: 2,
                severity: 1,
                message: "boom".into(),
                source: Some("rustc".into()),
            },
        ];
        let all = format_diagnostics(&diags, false);
        assert!(all.contains("warn") && all.contains("boom"));
        let errs = format_diagnostics(&diags, true);
        assert!(errs.contains("boom") && !errs.contains("warn"));
        assert!(errs.contains("[rustc]"));
    }
}
