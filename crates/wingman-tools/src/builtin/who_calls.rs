//! `who_calls`: find *references* to a symbol (call sites, mentions) across
//! the tree, each annotated with the enclosing function/method it appears in.
//!
//! This is the reference-side complement to `find_symbol` (which locates the
//! *definition*). The value over a plain `grep` is the enclosing-symbol
//! annotation: you learn *which* function calls the target, not just the raw
//! line. Answers "who uses this?" in one shot instead of 3–5 grep→read turns.
//!
//! Resolution order, first non-empty answer wins, and the output's first line
//! names the method that produced it:
//! 1. `callHierarchy/incomingCalls` from the language server for the file the
//!    symbol is defined in (resolved callers, named by the server);
//! 2. `textDocument/references` from that server (resolved, but mentions as
//!    well as calls);
//! 3. a whole-word name match over the tree. It can over-report (same name,
//!    different symbol) and can't see dynamic/aliased calls.
//!
//! The server is asked at the definitions tree-sitter finds, so a symbol
//! tree-sitter can't see a definition for goes straight to the name match.

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_lsp::{LspClient, LspError, Position};

pub struct WhoCalls;

#[derive(Debug, Deserialize)]
struct Args {
    /// Symbol name to find references to. Case-sensitive whole-word match.
    name: String,
    /// Glob to restrict the search (defaults to all source files).
    #[serde(default)]
    glob: Option<String>,
    /// Maximum results to return.
    #[serde(default)]
    limit: Option<u32>,
}

/// Definitions asked of a language server. Same-named symbols each get asked;
/// past this many the name is common enough that the wait isn't worth it.
const MAX_LSP_TARGETS: usize = 5;

const HEURISTIC: &str = "whole-word name match (heuristic: may include same-named symbols)";

/// A definition of the target name, found by tree-sitter.
struct Def {
    path: PathBuf,
    /// 1-based, as tree-sitter and the output report it.
    line: u32,
    /// Where the name itself sits, for the language server.
    pos: Position,
}

/// A reference site a language server returned.
struct Site {
    path: PathBuf,
    /// 0-based, as LSP reports it.
    line: u32,
    /// The enclosing function, when the server named it (call hierarchy).
    caller: Option<String>,
}

/// Byte offset of the first occurrence of `needle` in `line` as a whole
/// identifier token (not a substring of a longer identifier). Cheap
/// replacement for a real lexer.
fn find_word(line: &str, needle: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() {
        return None;
    }
    let is_ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut i = 0;
    while let Some(pos) = line[i..].find(needle) {
        let start = i + pos;
        let end = start + nb.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            return Some(start);
        }
        // Step past the match's first char, not byte, so the slice stays on
        // a char boundary for a non-ASCII name.
        i = start + needle.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// One pass over the tree: every definition of `needle` (anywhere, so a glob
/// restricting *callers* doesn't hide the callee) and up to `limit`
/// name-match rows from files the glob admits.
fn walk(
    root: &Path,
    fs: &dyn crate::filesystem::FileSystem,
    needle: &str,
    matcher: Option<&globset::GlobMatcher>,
    limit: usize,
) -> (Vec<Def>, Vec<String>) {
    let mut defs: Vec<Def> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(root).build();
    for entry in walker.flatten() {
        if out.len() >= limit && defs.len() >= MAX_LSP_TARGETS {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let Some(lang) = wingman_ts::Language::from_path(path) else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let Ok(bytes) = fs.read_blocking(path) else {
            continue;
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        // No mention at all: nothing to define or reference here, and no
        // reason to pay for a parse.
        if !text.contains(needle) {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();

        // Lines that are the *definition* of `needle` — skip them so
        // who_calls reports only references, not the declaration.
        let mut def_lines: HashSet<u32> = HashSet::new();
        for s in wingman_ts::extract_symbols(lang, &text)
            .into_iter()
            .filter(|s| s.name == needle)
        {
            def_lines.insert(s.start_line);
            // The name sits on the first line of the declaration that
            // mentions it (attributes and decorators can come before it).
            let found = (s.start_line..=s.end_line).find_map(|l| {
                let line = lines.get((l as usize).checked_sub(1)?)?;
                find_word(line, needle).map(|b| (l, line[..b].encode_utf16().count() as u32))
            });
            if let Some((l, character)) = found {
                defs.push(Def {
                    path: path.to_path_buf(),
                    line: l,
                    pos: Position {
                        line: l - 1,
                        character,
                    },
                });
            }
        }

        if matcher.is_some_and(|m| !m.is_match(&rel_str)) {
            continue;
        }
        for (idx, line) in lines.iter().enumerate() {
            if out.len() >= limit {
                break;
            }
            let lineno = idx as u32 + 1;
            if def_lines.contains(&lineno) || find_word(line, needle).is_none() {
                continue;
            }
            let enclosing = wingman_ts::enclosing_symbol(lang, &text, lineno)
                .filter(|s| s.name != needle)
                .map(|s| format!("  [in {} {}]", s.kind.label(), s.name))
                .unwrap_or_default();
            out.push(format!(
                "{}:{}{}  {}",
                rel_str,
                lineno,
                enclosing,
                line.trim()
            ));
        }
    }
    (defs, out)
}

/// Ask each target's language server who calls it: call hierarchy first, then
/// references. `None` means fall back to the name match.
///
/// An empty resolved answer also falls through: a cold server still indexing
/// answers "nothing" as confidently as a warm one with no callers, and the
/// name match over-reports rather than hiding real callers.
async fn lsp_sites(
    targets: &[(Arc<LspClient>, PathBuf, Position)],
) -> Option<(&'static str, Vec<Site>)> {
    // A `Server` error is the server declining the method (not supported);
    // anything else — timeout, closed pipe — means it is not answering, and
    // waiting out the next request's timeout too would only delay the fallback.
    let mut calls = Vec::new();
    for (client, path, pos) in targets {
        let items = match client.prepare_call_hierarchy(path, *pos).await {
            Ok(items) => items,
            Err(LspError::Server(_)) => continue,
            Err(_) => return None,
        };
        for item in &items {
            match client.incoming_calls(item).await {
                Ok(found) => calls.extend(found.into_iter().map(|c| Site {
                    path: c.at.path,
                    line: c.at.line,
                    caller: Some(c.caller),
                })),
                Err(LspError::Server(_)) => {}
                Err(_) => return None,
            }
        }
    }
    if !calls.is_empty() {
        return Some(("LSP callHierarchy/incomingCalls (resolved callers)", calls));
    }

    let mut refs = Vec::new();
    for (client, path, pos) in targets {
        match client.references(path, *pos, false).await {
            Ok(locs) => refs.extend(locs.into_iter().map(|l| Site {
                path: l.path,
                line: l.line,
                caller: None,
            })),
            Err(LspError::Server(_)) => {}
            Err(_) => return None,
        }
    }
    if !refs.is_empty() {
        return Some(("LSP textDocument/references (resolved references)", refs));
    }
    None
}

/// Render server-returned sites in the same `path:line  [in …]  source` shape
/// as the name match, dropping any the glob or the read policy excludes — a
/// server names whatever files it likes, and the source line is read here.
fn render_sites(
    ctx: &ToolCtx,
    sites: &[Site],
    defs: &[Def],
    matcher: Option<&globset::GlobMatcher>,
    needle: &str,
    limit: usize,
) -> Vec<String> {
    let mut seen: HashSet<(&Path, u32)> = HashSet::new();
    let mut out = Vec::new();
    for site in sites {
        if out.len() >= limit {
            break;
        }
        let lineno = site.line + 1;
        if !seen.insert((&site.path, lineno))
            || defs.iter().any(|d| d.path == site.path && d.line == lineno)
            || !ctx.allows_read(&site.path)
        {
            continue;
        }
        let rel_str = super::lsp_tools::rel(&ctx.project_root, &site.path);
        if matcher.is_some_and(|m| !m.is_match(&rel_str)) {
            continue;
        }
        let Ok(text) = ctx.fs.read_to_string_blocking(&site.path) else {
            continue;
        };
        let source = text.lines().nth(site.line as usize).unwrap_or("").trim();
        let enclosing = match &site.caller {
            Some(name) => Some(format!("  [in {name}]")),
            None => wingman_ts::Language::from_path(&site.path).and_then(|lang| {
                wingman_ts::enclosing_symbol(lang, &text, lineno)
                    .filter(|s| s.name != needle)
                    .map(|s| format!("  [in {} {}]", s.kind.label(), s.name))
            }),
        }
        .unwrap_or_default();
        out.push(format!("{rel_str}:{lineno}{enclosing}  {source}"));
    }
    out
}

#[async_trait]
impl Tool for WhoCalls {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "who_calls".into(),
            description: "Find *references* to a function/struct/method by name (call sites, mentions), \
                          each annotated with the enclosing symbol it appears in. Unlike `grep`, tells you \
                          *which* function contains each reference. Skips the definition line itself. \
                          When a language server is installed for the defining file, answers from its call \
                          hierarchy (or resolved references); otherwise a whole-word name match over rust, \
                          python, javascript, typescript, tsx, go. The first line names the method used. \
                          Returns `path:line  [in enclosing]  <source line>` rows."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact symbol name (case-sensitive, whole word)." },
                    "glob": { "type": "string", "description": "Optional glob to restrict the search (e.g. \"crates/**/*.rs\")." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let limit = args.limit.unwrap_or(50).clamp(1, 200) as usize;

        let matcher = match args.glob.as_deref() {
            Some(g) => match globset::Glob::new(g) {
                Ok(gl) => Some(gl.compile_matcher()),
                Err(e) => return ToolOutcome::err(format!("bad glob: {e}")),
            },
            None => None,
        };

        // Cloned into the blocking closure: the tree walk reads through
        // the seam like every other tool, using its blocking flavour.
        let (defs, hits) = {
            let root = ctx.project_root.clone();
            let fs = ctx.fs.clone();
            let needle = args.name.clone();
            let matcher = matcher.clone();
            tokio::task::spawn_blocking(move || walk(&root, &*fs, &needle, matcher.as_ref(), limit))
                .await
                .unwrap_or_default()
        };

        // `client_for` read-gates the path and yields nothing when no server
        // is installed (cached, so this is cheap after the first probe).
        let mut targets = Vec::new();
        for d in defs.iter().take(MAX_LSP_TARGETS) {
            if let Ok((abs, client)) =
                super::lsp_tools::client_for(ctx, &d.path.to_string_lossy()).await
            {
                targets.push((client, abs, d.pos));
            }
        }

        let (method, rows) = match lsp_sites(&targets).await {
            Some((method, sites)) => {
                let ctx = ctx.clone();
                let needle = args.name.clone();
                let rows = tokio::task::spawn_blocking(move || {
                    render_sites(&ctx, &sites, &defs, matcher.as_ref(), &needle, limit)
                })
                .await
                .unwrap_or_default();
                (method, rows)
            }
            None => (HEURISTIC, hits),
        };

        if rows.is_empty() {
            return ToolOutcome::ok(format!(
                "(no references to `{}` found via {method} — check spelling, or it may be defined but unused)",
                args.name
            ));
        }
        ToolOutcome::ok(format!("(via {method})\n{}", rows.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use wingman_config::PermissionMode;

    #[test]
    fn whole_word_matching() {
        assert_eq!(find_word("    foo();", "foo"), Some(4));
        assert!(find_word("let x = foo(bar);", "foo").is_some());
        assert!(find_word("let x = foobar();", "foo").is_none()); // substring, not word
        assert!(find_word("let foo_bar = 1;", "foo").is_none()); // ident continues
        assert!(find_word("a.foo", "foo").is_some()); // dot is a boundary
        assert_eq!(find_word("foobar(foo)", "foo"), Some(7)); // skips the non-word hit
        assert!(find_word("", "foo").is_none());
        assert_eq!(find_word("xé é", "é"), Some(4)); // non-ASCII: no mid-char slice
        assert!(find_word("nothing here", "foo").is_none());
    }

    type Handler = fn(&str, &Value, &Path) -> Result<Value, String>;

    /// A language server in a task, over an in-memory pipe: answers each
    /// request with `handler(method, params, root)` (an `Err` becomes a
    /// JSON-RPC error) and ignores notifications.
    async fn fake_server(root: &Path, handler: Handler) -> Arc<LspClient> {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let (client_r, client_w) = tokio::io::split(client_io);
        let (server_r, mut server_w) = tokio::io::split(server_io);
        let server_root = root.to_path_buf();
        tokio::spawn(async move {
            let mut reader = BufReader::new(server_r);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).await.unwrap();
                let msg: Value = serde_json::from_slice(&body).unwrap();
                let (Some(id), Some(method)) = (msg.get("id"), msg["method"].as_str()) else {
                    continue;
                };
                let reply = match handler(method, &msg["params"], &server_root) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(message) => json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": message } }),
                };
                let body = serde_json::to_vec(&reply).unwrap();
                let header = format!("Content-Length: {}\r\n\r\n", body.len());
                server_w.write_all(header.as_bytes()).await.unwrap();
                server_w.write_all(&body).await.unwrap();
            }
        });
        let no_writes = Arc::new(std::sync::RwLock::new(None));
        LspClient::connect(root, wingman_lsp::Lang::Rust, no_writes, client_r, client_w)
            .await
            .unwrap()
    }

    fn range(line: u32) -> Value {
        json!({ "start": { "line": line, "character": 4 }, "end": { "line": line, "character": 7 } })
    }

    /// `a.rs` defines `foo`; `b.rs` calls it from `caller`. Returns the
    /// definition as the walk finds it.
    fn project(dir: &Path) -> Vec<Def> {
        std::fs::write(dir.join("a.rs"), "pub fn foo() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "fn caller() {\n    foo();\n}\n").unwrap();
        let (defs, hits) = walk(dir, &crate::filesystem::OsFileSystem, "foo", None, 50);
        assert_eq!(hits, vec!["b.rs:2  [in fn caller]  foo();"]);
        defs
    }

    async fn ask(
        dir: &Path,
        defs: &[Def],
        handler: Handler,
    ) -> Option<(&'static str, Vec<String>)> {
        let client = fake_server(dir, handler).await;
        let targets: Vec<_> = defs
            .iter()
            .map(|d| (client.clone(), d.path.clone(), d.pos))
            .collect();
        let (method, sites) = lsp_sites(&targets).await?;
        let ctx = ToolCtx::new(
            PermissionMode::ReadOnly,
            dir.to_path_buf(),
            dir.to_path_buf(),
        );
        Some((method, render_sites(&ctx, &sites, defs, None, "foo", 50)))
    }

    #[test]
    fn walk_locates_the_name_on_its_definition() {
        let dir = tempfile::tempdir().unwrap();
        let defs = project(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(
            (defs[0].line, defs[0].pos.line, defs[0].pos.character),
            (1, 0, 7)
        );
    }

    #[tokio::test]
    async fn call_hierarchy_answers_first() {
        let dir = tempfile::tempdir().unwrap();
        let defs = project(dir.path());
        let (method, rows) = ask(dir.path(), &defs, |method, params, root| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            "textDocument/prepareCallHierarchy" => {
                assert_eq!(params["position"], json!({ "line": 0, "character": 7 }));
                Ok(json!([{ "name": "foo", "uri": params["textDocument"]["uri"] }]))
            }
            "callHierarchy/incomingCalls" => {
                assert_eq!(params["item"]["name"], "foo");
                let uri = wingman_lsp::client::path_to_uri(&root.join("b.rs"));
                Ok(json!([{ "from": { "name": "caller", "uri": uri }, "fromRanges": [range(1)] }]))
            }
            other => Err(format!("unexpected {other}")),
        })
        .await
        .expect("call hierarchy answer");
        assert!(method.contains("callHierarchy"), "{method}");
        assert_eq!(rows, vec!["b.rs:2  [in caller]  foo();"]);
    }

    #[tokio::test]
    async fn references_answer_when_call_hierarchy_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let defs = project(dir.path());
        let (method, rows) = ask(dir.path(), &defs, |method, params, root| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            "textDocument/references" => {
                assert_eq!(params["context"]["includeDeclaration"], false);
                let b = wingman_lsp::client::path_to_uri(&root.join("b.rs"));
                // A duplicate and the declaration itself are both dropped.
                let a = wingman_lsp::client::path_to_uri(&root.join("a.rs"));
                Ok(json!([
                    { "uri": b, "range": range(1) },
                    { "uri": b, "range": range(1) },
                    { "uri": a, "range": range(0) }
                ]))
            }
            other => Err(format!("method not found: {other}")),
        })
        .await
        .expect("references answer");
        assert!(method.contains("references"), "{method}");
        assert_eq!(rows, vec!["b.rs:2  [in fn caller]  foo();"]);
    }

    #[tokio::test]
    async fn no_resolved_answer_falls_back_to_the_name_match() {
        let dir = tempfile::tempdir().unwrap();
        let defs = project(dir.path());
        // Declines everything but the handshake.
        let answer = ask(dir.path(), &defs, |method, _, _| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            other => Err(format!("method not found: {other}")),
        })
        .await;
        assert!(answer.is_none());
        // Nothing to ask either.
        assert!(lsp_sites(&[]).await.is_none());
    }
}
