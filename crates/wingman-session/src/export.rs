//! Session export: a transcript reduced to what someone reviewing the work
//! asks about — what was asked and answered, which files changed and by how
//! much, whether verification passed, what it cost, and which tools ran.
//!
//! Everything is derived from the session's own records, so an export of a
//! session from last month reads the same as one of the session just closed:
//!
//! - **Files changed** come from the successful mutating tool calls. The
//!   `edit_file`/`edit_symbol` results are unified diffs and are counted
//!   line by line; `apply_patch` is counted from its patch; `write_file`
//!   counts the lines it wrote (what it replaced is not in the log). Undo
//!   checkpoints are not consulted: they are per-repo rather than per-session
//!   and an `/undo` deletes them.
//! - **Receipts** are each red gate report the loop fed back to the model
//!   (recorded as a `[wingman verify]` prompt, with its output) plus each
//!   turn's final verdict from its `stop` record.
//! - **Cost** prices each usage delta at the model the most recent
//!   `session_start` named, like the cost timeline.
//!
//! Every string that came from the model, a tool or the user goes through
//! [`redact_output_secrets`] before it lands in the export, and the count is
//! reported, because an export exists to be pasted somewhere else.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use wingman_core::redact::redact_output_secrets;
use wingman_core::{price_for, ContentBlock, Usage};

use crate::{load_session, SessionError, SessionRecord};

/// What the loop prefixes a failed gate report with when it feeds it back.
const VERIFY_PREFIX: &str = "[wingman verify]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Md,
    Html,
    Json,
}

impl FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Ok(Format::Md),
            "html" => Ok(Format::Html),
            "json" => Ok(Format::Json),
            other => Err(format!(
                "unknown export format '{other}' (expected md, html or json)"
            )),
        }
    }
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Html => "html",
            Format::Json => "json",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Md => "text/markdown; charset=utf-8",
            Format::Html => "text/html; charset=utf-8",
            Format::Json => "application/json",
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct FileChange {
    pub path: String,
    /// Successful mutating tool calls that touched this file.
    pub edits: u32,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Receipt {
    /// 1-based user turn the receipt belongs to.
    pub turn: u32,
    pub passed: bool,
    /// The gate's report, when the log kept it (red reports only).
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolCall {
    pub ts: String,
    pub turn: u32,
    pub name: String,
    /// The argument that says what the call was about: a path, a command, a
    /// pattern. Truncated.
    pub target: Option<String>,
    /// `None` when the log has no result for the call (an interrupted turn).
    pub ok: Option<bool>,
}

/// What `wingman session export --format json` prints.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SessionExport {
    pub session_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
    pub turns: u32,
    pub last_stop: Option<String>,
    /// The first prompt.
    pub task: Option<String>,
    /// The last thing the assistant said.
    pub outcome: Option<String>,
    pub files: Vec<FileChange>,
    pub receipts: Vec<Receipt>,
    pub usage: Usage,
    pub total_tokens: u64,
    /// Estimated spend; `None` when no usage could be priced.
    pub usd: Option<f64>,
    /// Usage deltas billed at a model with no known price.
    pub unpriced_usage: u32,
    pub tool_calls: Vec<ToolCall>,
    /// Secrets replaced with `[redacted-secret]` while building this export.
    pub redacted: usize,
}

/// Load the transcript at `path` and reduce it to an export named after the
/// file, which is how every surface — `wingman session export`, the TUI's
/// `/export` and the HTTP route — reaches it.
pub fn export_file(path: &Path) -> Result<SessionExport, SessionError> {
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(build(&id, &load_session(path)?))
}

/// Reduce `records` to an export. Never fails: a record the reducer does not
/// care about is skipped, and missing sections render as absences.
pub fn build(session_id: &str, records: &[SessionRecord]) -> SessionExport {
    let mut redacted = 0usize;
    let mut clean = |s: &str| {
        let (out, n) = redact_output_secrets(s);
        redacted += n;
        out
    };

    let mut x = SessionExport {
        session_id: session_id.to_string(),
        ..Default::default()
    };
    let mut files: BTreeMap<String, FileChange> = BTreeMap::new();
    // Tool-use id → (index into `tool_calls`, name, input), until its result.
    let mut open: HashMap<String, (usize, String, serde_json::Value)> = HashMap::new();

    for record in records {
        let ts = record_ts(record);
        if x.started.is_none() {
            x.started = Some(ts.to_string());
        }
        x.ended = Some(ts.to_string());
        let turn = x.turns + 1;
        match record {
            SessionRecord::SessionStart {
                model, provider, ..
            } => {
                x.model = Some(model.clone());
                x.provider = Some(provider.clone());
            }
            SessionRecord::User { text, .. } => {
                if let Some(report) = text.strip_prefix(VERIFY_PREFIX) {
                    x.receipts.push(Receipt {
                        turn,
                        passed: false,
                        detail: Some(clean(report.trim())),
                    });
                } else if x.task.is_none() {
                    x.task = Some(clean(text));
                }
            }
            SessionRecord::Assistant { blocks, .. } => {
                let text: Vec<&str> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    x.outcome = Some(clean(&text.join("\n\n")));
                }
                for b in blocks {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        open.insert(
                            id.clone(),
                            (x.tool_calls.len(), name.clone(), input.clone()),
                        );
                        x.tool_calls.push(ToolCall {
                            ts: ts.to_string(),
                            turn,
                            name: name.clone(),
                            target: target(input).map(|t| clean(&t)),
                            ok: None,
                        });
                    }
                }
            }
            SessionRecord::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                let Some((index, name, input)) = open.remove(id) else {
                    continue;
                };
                x.tool_calls[index].ok = Some(!is_error);
                if !is_error {
                    for (path, added, removed) in changes(&name, &input, output) {
                        let entry = files.entry(clean(&path)).or_default();
                        entry.edits += 1;
                        entry.added += added;
                        entry.removed += removed;
                    }
                }
            }
            SessionRecord::UsageDelta { usage, .. } => {
                add_usage(&mut x.usage, usage);
                match x.model.as_deref().and_then(price_for) {
                    Some(price) => *x.usd.get_or_insert(0.0) += price.cost(usage),
                    None => x.unpriced_usage += 1,
                }
            }
            SessionRecord::Stop {
                reason, verified, ..
            } => {
                x.turns += 1;
                // Older loop builds wrote the reason JSON-quoted.
                let reason = reason.trim_matches('"');
                x.last_stop = Some(reason.to_string());
                if let Some(passed) = verified {
                    // The gate runs right before an `end_turn` or `gate_failed`
                    // stop. Any other stop carries the verdict of the last
                    // report fed back this turn — already a receipt.
                    let fresh = matches!(reason, "end_turn" | "gate_failed");
                    let reported = x.receipts.last().is_some_and(|r| r.turn == turn);
                    if fresh || !reported {
                        x.receipts.push(Receipt {
                            turn,
                            passed: *passed,
                            detail: None,
                        });
                    }
                }
            }
            SessionRecord::Recap { .. }
            | SessionRecord::ToolResultPruned { .. }
            | SessionRecord::InjectedContext { .. } => {}
        }
    }

    let u = &x.usage;
    x.total_tokens = u.input_tokens as u64
        + u.output_tokens as u64
        + u.cache_read_input_tokens as u64
        + u.cache_creation_input_tokens as u64;
    x.files = files
        .into_iter()
        .map(|(path, f)| FileChange { path, ..f })
        .collect();
    x.redacted = redacted;
    x
}

fn record_ts(record: &SessionRecord) -> &str {
    match record {
        SessionRecord::SessionStart { ts, .. }
        | SessionRecord::User { ts, .. }
        | SessionRecord::Assistant { ts, .. }
        | SessionRecord::ToolResult { ts, .. }
        | SessionRecord::Recap { ts, .. }
        | SessionRecord::ToolResultPruned { ts, .. }
        | SessionRecord::InjectedContext { ts, .. }
        | SessionRecord::UsageDelta { ts, .. }
        | SessionRecord::Stop { ts, .. } => ts,
    }
}

fn add_usage(total: &mut Usage, u: &Usage) {
    total.input_tokens += u.input_tokens;
    total.output_tokens += u.output_tokens;
    total.cache_read_input_tokens += u.cache_read_input_tokens;
    total.cache_creation_input_tokens += u.cache_creation_input_tokens;
}

/// The one argument that names what a tool call was about.
fn target(input: &serde_json::Value) -> Option<String> {
    let value = ["path", "command", "pattern", "query", "url", "name"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()))?;
    let line = value.lines().next().unwrap_or_default();
    Some(if line.chars().count() > 80 || line.len() < value.len() {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_string()
    })
}

/// `(path, lines added, lines removed)` for each file a successful tool call
/// changed. Empty for tools that change nothing.
fn changes(name: &str, input: &serde_json::Value, output: &str) -> Vec<(String, u32, u32)> {
    let path = || {
        input
            .get("path")
            .and_then(|p| p.as_str())
            .map(str::to_string)
    };
    match name {
        // The result is a unified diff of the whole file.
        "edit_file" | "edit_symbol" => path()
            .map(|p| {
                let (mut added, mut removed) = (0, 0);
                for (i, line) in output.lines().enumerate() {
                    if i < 2 && (line.starts_with("--- ") || line.starts_with("+++ ")) {
                        continue;
                    }
                    if line.starts_with('+') {
                        added += 1;
                    } else if line.starts_with('-') {
                        removed += 1;
                    }
                }
                vec![(p, added, removed)]
            })
            .unwrap_or_default(),
        "write_file" => path()
            .map(|p| {
                let lines = input
                    .get("content")
                    .and_then(|c| c.as_str())
                    .map_or(0, |c| c.lines().count() as u32);
                vec![(p, lines, 0)]
            })
            .unwrap_or_default(),
        "apply_patch" => input
            .get("patch")
            .and_then(|p| p.as_str())
            .map(patch_changes)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Per-file line counts of an `apply_patch` patch, read the way the tool
/// parses it: inside an update, `- `/`+ ` (or a bare `-`/`+`) mark removed
/// and added lines and anything else is context; every line of an added file
/// is content; lines outside a file block are ignored.
fn patch_changes(patch: &str) -> Vec<(String, u32, u32)> {
    let mut out: Vec<(String, u32, u32)> = Vec::new();
    // `Some(adding)` while inside an Update (`false`) or Add (`true`) block.
    let mut block: Option<bool> = None;
    for line in patch.lines() {
        let trimmed = line.trim();
        let marked = |sign: &str| {
            line.strip_prefix(sign)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
        };
        if let Some(p) = trimmed.strip_prefix("*** Update File: ") {
            out.push((p.trim().to_string(), 0, 0));
            block = Some(false);
        } else if let Some(p) = trimmed.strip_prefix("*** Add File: ") {
            out.push((p.trim().to_string(), 0, 0));
            block = Some(true);
        } else if let Some(p) = trimmed.strip_prefix("*** Delete File: ") {
            out.push((p.trim().to_string(), 0, 0));
            block = None;
        } else if trimmed == "*** End File" {
            block = None;
        } else if let (Some(adding), Some(current)) = (block, out.last_mut()) {
            if adding || marked("+") {
                current.1 += 1;
            } else if marked("-") {
                current.2 += 1;
            }
        }
    }
    out
}

impl SessionExport {
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Md => self.markdown(),
            Format::Html => self.html(),
            Format::Json => serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into()),
        }
    }

    fn model_line(&self) -> String {
        match (&self.provider, &self.model) {
            (Some(p), Some(m)) => format!("{p} / {m}"),
            (None, Some(m)) => m.clone(),
            _ => "unknown".into(),
        }
    }

    fn cost_line(&self) -> String {
        let u = &self.usage;
        let usd = match self.usd {
            Some(usd) if self.unpriced_usage > 0 => {
                format!("${usd:.4} (plus {} unpriced turns)", self.unpriced_usage)
            }
            Some(usd) => format!("${usd:.4}"),
            None => "unpriced".into(),
        };
        format!(
            "{usd} · {} tokens (input {} · output {} · cache read {} · cache write {})",
            self.total_tokens,
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens
        )
    }

    /// `+a −r across n files`.
    pub fn files_line(&self) -> String {
        let added: u32 = self.files.iter().map(|f| f.added).sum();
        let removed: u32 = self.files.iter().map(|f| f.removed).sum();
        let n = self.files.len();
        format!(
            "+{added} −{removed} across {n} {}",
            if n == 1 { "file" } else { "files" }
        )
    }

    /// `2 of 3 green, last passed` — or that the gate never ran.
    pub fn receipts_line(&self) -> String {
        match self.receipts.last() {
            None => "the verification gate did not run".into(),
            Some(last) => format!(
                "{} of {} green, last {}",
                self.receipts.iter().filter(|r| r.passed).count(),
                self.receipts.len(),
                if last.passed { "passed" } else { "failed" }
            ),
        }
    }

    fn markdown(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# Session `{}`\n", self.session_id);
        let _ = writeln!(s, "- **Model:** {}", self.model_line());
        if let (Some(a), Some(b)) = (&self.started, &self.ended) {
            let _ = writeln!(s, "- **When:** {a} → {b}");
        }
        let _ = writeln!(
            s,
            "- **Turns:** {}{}",
            self.turns,
            self.last_stop
                .as_ref()
                .map(|r| format!(" (last stop: {r})"))
                .unwrap_or_default()
        );
        let _ = writeln!(s, "- **Cost:** {}", self.cost_line());
        let _ = writeln!(s, "- **Verification:** {}", self.receipts_line());
        let _ = writeln!(s, "- **Files:** {}", self.files_line());
        if self.redacted > 0 {
            let _ = writeln!(s, "- **Redacted:** {} secret(s)", self.redacted);
        }

        let _ = writeln!(s, "\n## Summary\n");
        let _ = writeln!(s, "**Task**\n");
        let _ = writeln!(s, "{}\n", quote(self.task.as_deref().unwrap_or("(none)")));
        let _ = writeln!(s, "**Outcome**\n");
        let _ = writeln!(
            s,
            "{}\n",
            quote(self.outcome.as_deref().unwrap_or("(none)"))
        );

        let _ = writeln!(s, "## Files changed\n");
        if self.files.is_empty() {
            let _ = writeln!(s, "No file edits were recorded.\n");
        } else {
            let _ = writeln!(s, "| File | Edits | + | − |\n|---|---:|---:|---:|");
            for f in &self.files {
                let _ = writeln!(
                    s,
                    "| `{}` | {} | {} | {} |",
                    cell(&f.path),
                    f.edits,
                    f.added,
                    f.removed
                );
            }
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "## Verification receipts\n");
        if self.receipts.is_empty() {
            let _ = writeln!(s, "The verification gate did not run in this session.\n");
        } else {
            for r in &self.receipts {
                let verdict = if r.passed { "✓ passed" } else { "✗ failed" };
                let _ = writeln!(s, "- turn {}: {verdict}", r.turn);
                if let Some(detail) = &r.detail {
                    // Indented, not fenced: gate output can contain fences.
                    let _ = writeln!(s);
                    for line in detail.lines() {
                        let _ = writeln!(s, "        {line}");
                    }
                    let _ = writeln!(s);
                }
            }
            let _ = writeln!(s);
        }

        let _ = writeln!(s, "## Tool calls\n");
        if self.tool_calls.is_empty() {
            let _ = writeln!(s, "No tools ran.");
        } else {
            let _ = writeln!(
                s,
                "| # | Turn | Tool | Target | Result |\n|---:|---:|---|---|---|"
            );
            for (i, c) in self.tool_calls.iter().enumerate() {
                let _ = writeln!(
                    s,
                    "| {} | {} | `{}` | {} | {} |",
                    i + 1,
                    c.turn,
                    cell(&c.name),
                    c.target
                        .as_deref()
                        .map(|t| format!("`{}`", cell(t)))
                        .unwrap_or_default(),
                    result(c.ok)
                );
            }
        }
        s
    }

    fn html(&self) -> String {
        let mut s = String::new();
        let _ = write!(
            s,
            "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Session {id}</title>\
             <style>body{{font:14px/1.5 system-ui,sans-serif;max-width:960px;margin:2rem auto;padding:0 1rem}}\
             table{{border-collapse:collapse;width:100%}}td,th{{border-bottom:1px solid #ddd;padding:4px 8px;text-align:left;vertical-align:top}}\
             pre{{white-space:pre-wrap;background:#f6f6f6;padding:8px}}code{{font-size:13px}}</style></head><body>\n\
             <h1>Session <code>{id}</code></h1>\n<ul>",
            id = esc(&self.session_id)
        );
        let mut item = |k: &str, v: String| {
            let _ = write!(s, "<li><b>{k}:</b> {}</li>", esc(&v));
        };
        item("Model", self.model_line());
        if let (Some(a), Some(b)) = (&self.started, &self.ended) {
            item("When", format!("{a} → {b}"));
        }
        item(
            "Turns",
            format!(
                "{}{}",
                self.turns,
                self.last_stop
                    .as_ref()
                    .map(|r| format!(" (last stop: {r})"))
                    .unwrap_or_default()
            ),
        );
        item("Cost", self.cost_line());
        item("Verification", self.receipts_line());
        item("Files", self.files_line());
        if self.redacted > 0 {
            item("Redacted", format!("{} secret(s)", self.redacted));
        }
        let _ = write!(
            s,
            "</ul>\n<h2>Summary</h2>\n<h3>Task</h3><pre>{}</pre>\n<h3>Outcome</h3><pre>{}</pre>\n<h2>Files changed</h2>\n",
            esc(self.task.as_deref().unwrap_or("(none)")),
            esc(self.outcome.as_deref().unwrap_or("(none)"))
        );
        if self.files.is_empty() {
            s.push_str("<p>No file edits were recorded.</p>\n");
        } else {
            s.push_str("<table><tr><th>File</th><th>Edits</th><th>+</th><th>−</th></tr>");
            for f in &self.files {
                let _ = write!(
                    s,
                    "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    esc(&f.path),
                    f.edits,
                    f.added,
                    f.removed
                );
            }
            s.push_str("</table>\n");
        }
        s.push_str("<h2>Verification receipts</h2>\n");
        if self.receipts.is_empty() {
            s.push_str("<p>The verification gate did not run in this session.</p>\n");
        } else {
            s.push_str("<ul>");
            for r in &self.receipts {
                let _ = write!(
                    s,
                    "<li>turn {}: {}",
                    r.turn,
                    if r.passed { "✓ passed" } else { "✗ failed" }
                );
                if let Some(detail) = &r.detail {
                    let _ = write!(s, "<pre>{}</pre>", esc(detail));
                }
                s.push_str("</li>");
            }
            s.push_str("</ul>\n");
        }
        s.push_str("<h2>Tool calls</h2>\n");
        if self.tool_calls.is_empty() {
            s.push_str("<p>No tools ran.</p>\n");
        } else {
            s.push_str(
                "<table><tr><th>#</th><th>Turn</th><th>Tool</th><th>Target</th><th>Result</th></tr>",
            );
            for (i, c) in self.tool_calls.iter().enumerate() {
                let _ = write!(
                    s,
                    "<tr><td>{}</td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                    i + 1,
                    c.turn,
                    esc(&c.name),
                    esc(c.target.as_deref().unwrap_or_default()),
                    result(c.ok)
                );
            }
            s.push_str("</table>\n");
        }
        s.push_str("</body></html>\n");
        s
    }
}

fn result(ok: Option<bool>) -> &'static str {
    match ok {
        Some(true) => "ok",
        Some(false) => "error",
        None => "no result",
    }
}

/// A markdown blockquote of `text`.
fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text safe inside one markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(lines: &[&str]) -> Vec<SessionRecord> {
        lines
            .iter()
            .map(|l| serde_json::from_str(l).expect(l))
            .collect()
    }

    /// One session exercising every section: an edit, a patch, a write, a
    /// failed call, a red-then-green gate, priced usage and a leaked key.
    fn sample() -> Vec<SessionRecord> {
        records(&[
            r#"{"kind":"session_start","ts":"t0","model":"claude-sonnet-5","provider":"anthropic","system_hash":null}"#,
            r#"{"kind":"user","ts":"t1","text":"fix the parser; my key is sk-abcdefghij0123456789ABCDEF"}"#,
            r#"{"kind":"assistant","ts":"t2","blocks":[{"type":"text","text":"On it."},{"type":"tool_use","id":"a","name":"edit_file","input":{"path":"src/p.rs","old_string":"x","new_string":"y"}},{"type":"tool_use","id":"b","name":"apply_patch","input":{"patch":"*** Begin Patch\n*** Update File: src/p.rs\n@@\n- old\n+ new\n+ newer\n*** End File\n*** Add File: src/q.rs\n+ one\ntwo\n*** End File\n*** End Patch"}},{"type":"tool_use","id":"c","name":"run_shell","input":{"command":"cargo test | tail"}},{"type":"tool_use","id":"d","name":"write_file","input":{"path":"denied.rs","content":"a\nb"}}]}"#,
            r#"{"kind":"tool_result","ts":"t3","id":"a","output":"--- a/src/p.rs\n+++ b/src/p.rs\n fn f() {\n-    x\n+    y\n }\n","is_error":false}"#,
            r#"{"kind":"tool_result","ts":"t3","id":"b","output":"updated src/p.rs\nadded   src/q.rs\n","is_error":false}"#,
            r#"{"kind":"tool_result","ts":"t3","id":"c","output":"ok","is_error":false}"#,
            r#"{"kind":"tool_result","ts":"t3","id":"d","output":"denied","is_error":true}"#,
            r#"{"kind":"user","ts":"t4","text":"[wingman verify] Turn gate failed after your edits (cargo test). Fix the issues, then end the turn again.\n\ntest p ... FAILED"}"#,
            r#"{"kind":"usage_delta","ts":"t5","usage":{"input_tokens":1000000,"output_tokens":0}}"#,
            r#"{"kind":"assistant","ts":"t6","blocks":[{"type":"text","text":"Fixed | the parser."}]}"#,
            r#"{"kind":"stop","ts":"t7","reason":"end_turn","verified":true}"#,
        ])
    }

    #[test]
    fn a_session_reduces_to_its_sections() {
        let x = build("s1", &sample());
        assert_eq!(x.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            (x.started.as_deref(), x.ended.as_deref()),
            (Some("t0"), Some("t7"))
        );
        assert_eq!(x.turns, 1);
        assert_eq!(x.last_stop.as_deref(), Some("end_turn"));
        assert_eq!(x.outcome.as_deref(), Some("Fixed | the parser."));

        // The failed write is in the timeline but changed nothing.
        assert_eq!(
            x.files,
            vec![
                FileChange {
                    path: "src/p.rs".into(),
                    edits: 2,
                    added: 3,
                    removed: 2
                },
                FileChange {
                    path: "src/q.rs".into(),
                    edits: 1,
                    added: 2,
                    removed: 0
                },
            ]
        );
        assert_eq!(x.tool_calls.len(), 4);
        assert_eq!(x.tool_calls[2].target.as_deref(), Some("cargo test | tail"));
        assert_eq!(x.tool_calls[3].ok, Some(false));

        assert_eq!(x.receipts.len(), 2);
        assert!(!x.receipts[0].passed);
        assert!(x.receipts[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("test p ... FAILED"));
        assert!(x.receipts[1].passed);
        assert_eq!(x.receipts_line(), "1 of 2 green, last passed");

        assert_eq!(x.total_tokens, 1_000_000);
        assert!(x.usd.unwrap() > 0.0, "a known model is priced");
        assert_eq!(x.unpriced_usage, 0);
    }

    #[test]
    fn secrets_never_reach_any_format() {
        let x = build("s1", &sample());
        assert_eq!(x.redacted, 1);
        for format in [Format::Md, Format::Html, Format::Json] {
            let out = x.render(format);
            assert!(!out.contains("sk-abcdefghij"), "{format:?}: {out}");
            assert!(out.contains("[redacted-secret]"), "{format:?}");
        }
    }

    #[test]
    fn markdown_and_html_escape_what_would_break_them() {
        let x = build("s1", &sample());
        let md = x.render(Format::Md);
        assert!(md.contains("| `src/p.rs` | 2 | 3 | 2 |"), "{md}");
        assert!(md.contains("`cargo test \\| tail`"), "{md}");
        assert!(md.contains("- turn 1: ✗ failed"), "{md}");
        assert!(md.contains("        test p ... FAILED"), "{md}");

        let mut hostile = sample();
        hostile.push(SessionRecord::User {
            ts: "t8".into(),
            text: "ignored: only the first prompt is the task".into(),
        });
        hostile.insert(
            1,
            SessionRecord::User {
                ts: "t1".into(),
                text: "<script>alert(1)</script>".into(),
            },
        );
        let html = build("s1", &hostile).render(Format::Html);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");

        let json: serde_json::Value = serde_json::from_str(&x.render(Format::Json)).unwrap();
        assert_eq!(json["files"][0]["path"], "src/p.rs");
        assert_eq!(json["receipts"][1]["passed"], true);
    }

    /// A `gate_failed` stop is a fresh red run after the last fed-back report;
    /// a `max_turns` stop only repeats the verdict of the report before it.
    #[test]
    fn only_stops_the_gate_ran_before_add_receipts() {
        let x = build(
            "s",
            &records(&[
                r#"{"kind":"user","ts":"t","text":"go"}"#,
                r#"{"kind":"user","ts":"t","text":"[wingman verify] failed\n\nboom"}"#,
                r#"{"kind":"stop","ts":"t","reason":"gate_failed","verified":false}"#,
                r#"{"kind":"user","ts":"t","text":"again"}"#,
                r#"{"kind":"user","ts":"t","text":"[wingman verify] failed\n\nboom"}"#,
                r#"{"kind":"stop","ts":"t","reason":"max_turns","verified":false}"#,
                r#"{"kind":"user","ts":"t","text":"old log"}"#,
                r#"{"kind":"stop","ts":"t","reason":"\"end_turn\""}"#,
            ]),
        );
        assert_eq!(x.receipts.len(), 3);
        assert_eq!(
            x.receipts.iter().map(|r| r.turn).collect::<Vec<_>>(),
            vec![1, 1, 2]
        );
        assert_eq!(x.turns, 3);
        assert_eq!(x.last_stop.as_deref(), Some("end_turn"));
        assert_eq!(x.task.as_deref(), Some("go"));
    }

    #[test]
    fn an_empty_or_unpriced_session_reports_absences() {
        let x = build(
            "s",
            &records(&[
                r#"{"kind":"session_start","ts":"t","model":"no-such-model","provider":"p","system_hash":null}"#,
                r#"{"kind":"usage_delta","ts":"t","usage":{"input_tokens":5,"output_tokens":5}}"#,
            ]),
        );
        assert_eq!(x.usd, None);
        assert_eq!(x.unpriced_usage, 1);
        let md = x.render(Format::Md);
        assert!(md.contains("unpriced"), "{md}");
        assert!(md.contains("No file edits were recorded."), "{md}");
        assert!(md.contains("did not run"), "{md}");
        assert!(md.contains("No tools ran."), "{md}");
    }

    #[test]
    fn a_transcript_file_exports_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("20260914T101500000Z.jsonl");
        let lines: Vec<String> = sample()
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let x = export_file(&path).unwrap();
        assert_eq!(x.session_id, "20260914T101500000Z");
        assert_eq!(x.files.len(), 2);
        assert!(export_file(&dir.path().join("missing.jsonl")).is_err());
    }

    #[test]
    fn formats_parse_by_name_only() {
        assert_eq!("MD".parse::<Format>(), Ok(Format::Md));
        assert_eq!("html".parse::<Format>(), Ok(Format::Html));
        assert!("pdf".parse::<Format>().unwrap_err().contains("expected md"));
    }
}
