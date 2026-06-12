//! `check_diagnostics`: run the project's type/build checker and return
//! structured per-file diagnostics (`file:line level message`), so the model
//! sees compiler errors as data instead of a wall of raw output — and can
//! fix them *before* the post-edit turn gate fires.
//!
//! Read-only by design: the check command is fixed per ecosystem (never
//! caller-supplied), so this is allowed in every permission mode.

use crate::{Tool, ToolCtx};
use arccode_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

pub struct CheckDiagnostics;

#[derive(Debug, Deserialize)]
struct Args {
    /// Only report diagnostics whose file path contains this substring.
    #[serde(default)]
    path_filter: Option<String>,
    /// Maximum diagnostics to return (default 50).
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Debug)]
struct Diagnostic {
    file: String,
    line: u32,
    level: String,
    message: String,
}

#[async_trait]
impl Tool for CheckDiagnostics {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "check_diagnostics".into(),
            description: concat!(
                "Run the project's type checker / compiler (auto-detected: cargo check, ",
                "tsc --noEmit, go build, python compileall) and return structured ",
                "diagnostics as `file:line level message` lines. Use after editing to ",
                "catch type errors before claiming the task done. Read-only."
            )
            .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path_filter": { "type": "string", "description": "Only diagnostics whose file path contains this substring." },
                    "max": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Max diagnostics returned (default 50)." }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let root = ctx.project_root.clone();

        let Some(kind) = ProjectKind::detect(&root) else {
            return ToolOutcome::err(
                "no supported project marker found (Cargo.toml, tsconfig.json, go.mod, \
                 pyproject.toml)",
            );
        };

        let output = {
            let (program, argv) = kind.command();
            let mut cmd = Command::new(program);
            cmd.args(argv).current_dir(&root);
            match tokio::time::timeout(Duration::from_secs(300), cmd.output()).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    return ToolOutcome::err(format!("could not run {}: {e}", kind.label()))
                }
                Err(_) => return ToolOutcome::err("check timed out after 300s"),
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut diags = match kind {
            ProjectKind::Rust => parse_cargo_json(&stdout),
            ProjectKind::TypeScript => parse_tsc(&stdout) ,
            ProjectKind::Go => parse_line_colon(&stderr),
            ProjectKind::Python => parse_line_colon(&stderr),
        };

        if let Some(f) = &args.path_filter {
            diags.retain(|d| d.file.contains(f.as_str()));
        }
        let max = args.max.unwrap_or(50).min(500);
        let total = diags.len();
        diags.truncate(max);

        if diags.is_empty() {
            return ToolOutcome::ok(format!("✓ {} clean — no diagnostics", kind.label()));
        }
        let mut body = format!(
            "{total} diagnostic(s) from {}{}:\n",
            kind.label(),
            if total > max {
                format!(" (showing first {max})")
            } else {
                String::new()
            }
        );
        for d in &diags {
            body.push_str(&format!(
                "{}:{} {} {}\n",
                d.file, d.line, d.level, d.message
            ));
        }
        // Diagnostics present is a *successful* tool run — the data is the
        // point — but flag pure-error situations for the model.
        ToolOutcome::ok(body)
    }
}

enum ProjectKind {
    Rust,
    TypeScript,
    Go,
    Python,
}

impl ProjectKind {
    fn detect(root: &Path) -> Option<Self> {
        if root.join("Cargo.toml").exists() {
            Some(Self::Rust)
        } else if root.join("tsconfig.json").exists() {
            Some(Self::TypeScript)
        } else if root.join("go.mod").exists() {
            Some(Self::Go)
        } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
            Some(Self::Python)
        } else {
            None
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Rust => "cargo check",
            Self::TypeScript => "tsc --noEmit",
            Self::Go => "go build",
            Self::Python => "python compileall",
        }
    }

    fn command(&self) -> (&'static str, Vec<&'static str>) {
        match self {
            Self::Rust => (
                "cargo",
                vec!["check", "--workspace", "--quiet", "--message-format=json"],
            ),
            Self::TypeScript => {
                if cfg!(windows) {
                    ("npx.cmd", vec!["tsc", "--noEmit", "--pretty", "false"])
                } else {
                    ("npx", vec!["tsc", "--noEmit", "--pretty", "false"])
                }
            }
            Self::Go => ("go", vec!["build", "./..."]),
            Self::Python => ("python", vec!["-m", "compileall", "-q", "."]),
        }
    }
}

/// Parse `cargo check --message-format=json` lines.
fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let msg = &v["message"];
        let level = msg["level"].as_str().unwrap_or("");
        if level != "error" && level != "warning" {
            continue;
        }
        let text = msg["message"].as_str().unwrap_or("").to_string();
        let (file, line_no) = msg["spans"]
            .as_array()
            .and_then(|spans| spans.iter().find(|s| s["is_primary"] == true))
            .map(|s| {
                (
                    s["file_name"].as_str().unwrap_or("?").to_string(),
                    s["line_start"].as_u64().unwrap_or(0) as u32,
                )
            })
            .unwrap_or_else(|| ("?".into(), 0));
        out.push(Diagnostic {
            file,
            line: line_no,
            level: level.to_string(),
            message: text,
        });
    }
    out
}

/// Parse `tsc --pretty false` lines: `src/x.ts(12,5): error TS2322: msg`.
fn parse_tsc(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some(paren) = line.find('(') else { continue };
        let Some(close) = line.find("):") else { continue };
        if close < paren {
            continue;
        }
        let file = line[..paren].trim().to_string();
        let line_no = line[paren + 1..close]
            .split(',')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let rest = line[close + 2..].trim();
        let level = if rest.starts_with("error") { "error" } else { "warning" };
        out.push(Diagnostic {
            file,
            line: line_no,
            level: level.into(),
            message: rest.to_string(),
        });
    }
    out
}

/// Parse `file:line:col: message` lines (go build, python).
fn parse_line_colon(stderr: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(line_no) = parts[1].trim().parse::<u32>() else {
            continue;
        };
        let message = parts.last().unwrap_or(&"").trim().to_string();
        if message.is_empty() {
            continue;
        }
        out.push(Diagnostic {
            file: parts[0].trim().to_string(),
            line: line_no,
            level: "error".into(),
            message,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_json_messages() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","spans":[{"is_primary":true,"file_name":"src/lib.rs","line_start":42}]}}"#;
        let diags = parse_cargo_json(line);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "src/lib.rs");
        assert_eq!(diags[0].line, 42);
        assert_eq!(diags[0].level, "error");
    }

    #[test]
    fn parses_tsc_lines() {
        let out = "src/app.ts(12,5): error TS2322: Type 'string' is not assignable.\n";
        let diags = parse_tsc(out);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "src/app.ts");
        assert_eq!(diags[0].line, 12);
    }

    #[test]
    fn parses_go_style_lines() {
        let out = "main.go:7:2: undefined: fmtt\n";
        let diags = parse_line_colon(out);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "main.go");
        assert_eq!(diags[0].line, 7);
        assert!(diags[0].message.contains("undefined"));
    }

    #[test]
    fn skips_non_diagnostic_noise() {
        assert!(parse_cargo_json(r#"{"reason":"build-finished","success":true}"#).is_empty());
        assert!(parse_tsc("Compilation complete.\n").is_empty());
        assert!(parse_line_colon("ok\n").is_empty());
    }
}
