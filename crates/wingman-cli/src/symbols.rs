//! Symbol-level analysis over the project tree, for two callers:
//!
//! - the affected-tests gate ([`narrow_to_tests`]): which tests reference the
//!   symbols edited this turn, so only those run;
//! - `wingman knows` ([`defined_symbols`]): which symbols named in memories
//!   the project still defines, so memories naming deleted code read as stale.
//!
//! Both use a language server when one is installed and the tree-sitter index
//! otherwise, and both say which one answered. A server's empty answer falls
//! through to tree-sitter: a cold server still indexing answers "nothing" as
//! confidently as a warm one.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wingman_lsp::{LspClient, LspError, Position};

use crate::runtime::Narrowed;

/// Symbols put to a language server per run, one request each. Past this many
/// the tree-sitter index maps them in one walk instead.
const MAX_LSP_SYMBOLS: usize = 20;

/// Tests named on one `cargo test` line. More than this and the command line
/// gets long (cmd.exe caps at 8191 characters) for little saving.
const MAX_NARROWED_TESTS: usize = 64;

const VIA_LSP: &str = "LSP textDocument/references";
const VIA_TREE_SITTER: &str = "tree-sitter symbol index (name match in test code)";

/// A symbol whose definition overlaps a line changed this turn.
struct EditedSymbol {
    name: String,
    /// The defining file, absolute.
    path: PathBuf,
    /// Where the name sits in the definition, for a language server.
    pos: Position,
}

/// Byte offset of `word` in `line` as a whole identifier, not part of a longer
/// one.
fn find_word(line: &str, word: &str) -> Option<usize> {
    let ident = |c: char| c == '_' || c.is_alphanumeric();
    line.match_indices(word).map(|(i, _)| i).find(|&i| {
        !line[..i].chars().next_back().is_some_and(ident)
            && !line[i + word.len()..].chars().next().is_some_and(ident)
    })
}

/// Source files under `root` tree-sitter can parse, honoring `.gitignore`.
fn source_files(root: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| wingman_ts::Language::from_path(p).is_some())
        .collect()
}

/// Symbols whose definition overlaps a changed line, from the files `git
/// status` lists (`paths`) and the lines `git diff HEAD` touched (`changed`).
/// Impls and modules are left out: their members are symbols of their own,
/// and a change inside `mod tests` is not a change to everything that says
/// `tests`.
///
/// `Err` (naming the change) when some change inside a crate isn't inside one
/// of those symbols — a new, deleted or non-Rust file, a `use` line, a doc
/// comment (doc tests), a constant in an `impl` — since narrowing to the
/// tests of the symbols that were found would silently skip its tests.
fn edited_symbols(
    root: &Path,
    paths: &[String],
    changed: &std::collections::HashMap<String, BTreeSet<u32>>,
) -> Result<Vec<EditedSymbol>, String> {
    let mut out = Vec::new();
    for path in paths {
        if crate::runtime::crate_name_for(root, Path::new(path)).is_none() {
            continue;
        }
        let abs = root.join(path);
        let (Some(lines), Some(lang), Ok(text)) = (
            changed.get(path).filter(|_| path.ends_with(".rs")),
            wingman_ts::Language::from_path(&abs),
            std::fs::read_to_string(&abs),
        ) else {
            return Err(format!(
                "`{path}` changed but is not an edited Rust file `git diff HEAD` shows"
            ));
        };
        let mut covered = BTreeSet::new();
        let src: Vec<&str> = text.lines().collect();
        for sym in wingman_ts::extract_symbols(lang, &text) {
            if matches!(
                sym.kind,
                wingman_ts::SymbolKind::Impl | wingman_ts::SymbolKind::Module
            ) || lines.range(sym.start_line..=sym.end_line).next().is_none()
            {
                continue;
            }
            // The name sits on the first line of the declaration that
            // mentions it (attributes can come before it).
            let pos = (sym.start_line..=sym.end_line).find_map(|l| {
                let line = src.get((l as usize).checked_sub(1)?)?;
                let b = find_word(line, &sym.name)?;
                Some(Position {
                    line: l - 1,
                    character: line[..b].encode_utf16().count() as u32,
                })
            });
            if let Some(pos) = pos {
                covered.extend(sym.start_line..=sym.end_line);
                out.push(EditedSymbol {
                    name: sym.name,
                    path: abs.clone(),
                    pos,
                });
            }
        }
        if let Some(line) = lines.difference(&covered).next() {
            return Err(format!(
                "`{path}` line {line} changed outside any function or type"
            ));
        }
    }
    Ok(out)
}

/// Where the language server says each edited symbol is referenced, through
/// the pooled manager. `None` when the server can't give a complete answer:
/// too many symbols, no server, or any symbol it can't answer for.
async fn lsp_sites(root: &Path, symbols: &[EditedSymbol]) -> Option<Vec<(PathBuf, u32)>> {
    if symbols.len() > MAX_LSP_SYMBOLS {
        return None;
    }
    let manager = wingman_lsp::manager_for(root).await;
    let mut targets = Vec::new();
    for sym in symbols {
        targets.push((manager.client_for_path(&sym.path).await.ok()?, sym));
    }
    lsp_reference_sites(&targets).await
}

/// Where each target's language server says its symbol is referenced — the
/// declaration included, so an edited test maps to itself — as `(file,
/// 1-based line)`. Any error, or an empty answer (a warm server always
/// returns at least the declaration), voids the whole answer, since a partial
/// set of sites would narrow to too few tests.
async fn lsp_reference_sites(
    targets: &[(Arc<LspClient>, &EditedSymbol)],
) -> Option<Vec<(PathBuf, u32)>> {
    let mut sites = Vec::new();
    for (client, sym) in targets {
        let locs = client.references(&sym.path, sym.pos, true).await.ok()?;
        if locs.is_empty() {
            return None;
        }
        sites.extend(locs.into_iter().map(|l| (l.path, l.line + 1)));
    }
    Some(sites)
}

/// Every whole-word mention of an edited name in a Rust file that has test
/// code, as `(file, 1-based line)`: the tree-sitter tier's stand-in for a
/// server's references.
fn name_sites(root: &Path, names: &BTreeSet<String>) -> Vec<(PathBuf, u32)> {
    let mut sites = Vec::new();
    for path in source_files(root) {
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !names.iter().any(|n| text.contains(n.as_str())) {
            continue;
        }
        for (idx, line) in text.lines().enumerate() {
            if names.iter().any(|n| find_word(line, n).is_some()) {
                sites.push((path.clone(), idx as u32 + 1));
            }
        }
    }
    sites
}

/// Whether `line` of `rel` is test code: a file under `tests/` or named
/// `tests.rs`, or a line at or below the file's first `#[cfg(test)]`.
///
/// ponytail: relies on the convention that a unit-test module sits at the
/// bottom of its file; a `#[cfg(test)]` helper above production code makes
/// the code below it read as test code. Upgrade path is asking tree-sitter
/// for the attributes on the enclosing item.
fn in_test_code(rel: &Path, text: &str, line: u32) -> bool {
    rel.components().any(|c| c.as_os_str() == "tests")
        || rel.file_stem().is_some_and(|s| s == "tests")
        || text
            .lines()
            .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
            .is_some_and(|i| (i as u32) < line)
}

/// The functions (by name) that contain the test-code `sites`, and the crates
/// those files belong to. Sites outside `root` (a dependency, the standard
/// library) are dropped.
fn test_fns_at(root: &Path, sites: &[(PathBuf, u32)]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut by_file: BTreeMap<&Path, Vec<u32>> = BTreeMap::new();
    for (path, line) in sites {
        by_file.entry(path).or_default().push(*line);
    }
    // A server may report the root resolved through symlinks.
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut fns = BTreeSet::new();
    let mut crates = BTreeSet::new();
    for (path, lines) in by_file {
        let Ok(rel) = path
            .strip_prefix(root)
            .or_else(|_| path.strip_prefix(&canonical))
        else {
            continue;
        };
        let (Some(lang), Ok(text)) = (
            wingman_ts::Language::from_path(path),
            std::fs::read_to_string(path),
        ) else {
            continue;
        };
        let lines: Vec<u32> = lines
            .into_iter()
            .filter(|&l| in_test_code(rel, &text, l))
            .collect();
        if lines.is_empty() {
            continue;
        }
        let symbols = wingman_ts::extract_symbols(lang, &text);
        for line in lines {
            let innermost = symbols
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind,
                        wingman_ts::SymbolKind::Function | wingman_ts::SymbolKind::Method
                    ) && s.start_line <= line
                        && line <= s.end_line
                })
                .min_by_key(|s| s.end_byte - s.start_byte);
            if let Some(f) = innermost {
                fns.insert(f.name.clone());
                if let Some(c) = crate::runtime::crate_name_for(root, rel) {
                    crates.insert(c);
                }
            }
        }
    }
    (fns, crates)
}

/// Listed tests whose final path segment is one of `fns`. Names that aren't
/// plain `a::b::c` paths (doc tests, anything with spaces) are skipped, since
/// the result goes on a shell command line.
fn pick_tests(listed: Vec<String>, fns: &BTreeSet<String>) -> Vec<String> {
    listed
        .into_iter()
        .filter(|t| {
            t.chars()
                .all(|c| c == '_' || c == ':' || c.is_ascii_alphanumeric())
        })
        .filter(|t| fns.contains(t.rsplit("::").next().unwrap_or(t)))
        .collect()
}

/// Map this turn's edited symbols to the tests that reference them: through
/// the language server when it answers for every symbol, else the tree-sitter
/// index. Returns the edited symbol names, and either the narrowed tests or
/// why the whole changed crates should run.
///
/// ponytail: direct references only. A test that reaches the edit through
/// another function isn't mapped (when no test references an edit directly,
/// the whole crate runs, so the gap is only when some do). Upgrade path is
/// walking incoming calls transitively up to the tests.
pub(crate) async fn narrow_to_tests(
    root: &Path,
    crates: &[String],
) -> (Vec<String>, Result<Narrowed, String>) {
    let symbols = match edited_symbols(
        root,
        &crate::runtime::changed_paths(root),
        &crate::runtime::changed_lines_by_file(root),
    ) {
        Ok(s) if s.is_empty() => return (Vec::new(), Err("no edited symbol found".into())),
        Ok(s) => s,
        Err(why) => return (Vec::new(), Err(why)),
    };
    let names: BTreeSet<String> = symbols.iter().map(|s| s.name.clone()).collect();

    let (via, (fns, test_crates)) = match lsp_sites(root, &symbols).await {
        Some(sites) => (VIA_LSP, test_fns_at(root, &sites)),
        None => (
            VIA_TREE_SITTER,
            test_fns_at(root, &name_sites(root, &names)),
        ),
    };
    let names = names.into_iter().collect();
    if fns.is_empty() {
        return (
            names,
            Err(format!("no test references an edited symbol (via {via})")),
        );
    }

    let mut all: BTreeSet<String> = crates.iter().cloned().collect();
    all.extend(test_crates);
    let crates: Vec<String> = all.into_iter().collect();
    let pkg_flags: String = crates.iter().map(|c| format!(" -p {c}")).collect();
    let Some(listed) = crate::runtime::list_tests(root, &pkg_flags).await else {
        return (names, Err("could not list the tests".into()));
    };
    let tests = pick_tests(listed, &fns);
    let outcome = if tests.is_empty() {
        Err(format!(
            "no listed test references an edited symbol (via {via})"
        ))
    } else if tests.len() > MAX_NARROWED_TESTS {
        Err(format!(
            "{} tests reference an edited symbol (via {via}), too many to name",
            tests.len()
        ))
    } else {
        Ok(Narrowed { via, crates, tests })
    };
    (names, outcome)
}

/// Names put to language servers per `wingman knows` run.
const MAX_WORKSPACE_QUERIES: usize = 20;

/// Which of `names` the project still has, and a label naming what was
/// consulted. The tree-sitter index answers first: a name counts when a
/// source file defines it, or still mentions it as a whole word (a type the
/// project imports from a dependency, like `PathBuf`, is not stale). Names it
/// finds neither way go to each installed language server's
/// `workspace/symbol` before they are called stale, which also sees what
/// tree-sitter can't (macro-generated items).
///
/// ponytail: a mention in a comment keeps a deleted name alive. Upgrade path
/// is asking tree-sitter whether the mention is in a comment node.
pub(crate) async fn defined_symbols(
    root: &Path,
    names: &BTreeSet<String>,
) -> (HashSet<String>, &'static str) {
    let mut found = HashSet::new();
    // One file per language, to pick that language's server.
    let mut per_lang: BTreeMap<&'static str, PathBuf> = BTreeMap::new();
    for path in source_files(root) {
        if let Some(lang) = wingman_lsp::Lang::from_path(&path) {
            per_lang.entry(lang.label()).or_insert_with(|| path.clone());
        }
        let (Some(lang), Ok(text)) = (
            wingman_ts::Language::from_path(&path),
            std::fs::read_to_string(&path),
        ) else {
            continue;
        };
        if !names.iter().any(|n| text.contains(n.as_str())) {
            continue;
        }
        for sym in wingman_ts::extract_symbols(lang, &text) {
            if names.contains(&sym.name) {
                found.insert(sym.name);
            }
        }
        for name in names {
            if text.lines().any(|l| find_word(l, name).is_some()) {
                found.insert(name.clone());
            }
        }
    }

    let unresolved: Vec<&String> = names
        .iter()
        .filter(|n| !found.contains(*n))
        .take(MAX_WORKSPACE_QUERIES)
        .collect();
    if unresolved.is_empty() {
        return (found, "tree-sitter index");
    }
    let manager = wingman_lsp::manager_for(root).await;
    let mut clients = Vec::new();
    for file in per_lang.values() {
        if let Ok(client) = manager.client_for_path(file).await {
            clients.push(client);
        }
    }
    if clients.is_empty() {
        return (found, "tree-sitter index (no language server available)");
    }
    found.extend(workspace_matches(&clients, &unresolved).await);
    (found, "tree-sitter index, then LSP workspace/symbol")
}

/// The `names` some client's `workspace/symbol` knows by exactly that name. A
/// client that declines is asked the next name; one that times out or closes
/// its pipe is not asked again.
async fn workspace_matches(clients: &[Arc<LspClient>], names: &[&String]) -> HashSet<String> {
    let mut found = HashSet::new();
    for client in clients {
        for name in names {
            match client.workspace_symbols(name).await {
                Ok(matches) if matches.iter().any(|m| m == *name) => {
                    found.insert((*name).clone());
                }
                Ok(_) | Err(LspError::Server(_)) => {}
                Err(_) => break,
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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

    /// A crate `foo` whose `parse` is tested by `parses` in its unit-test
    /// module and used (not tested) by `main`.
    fn project(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn parse() -> u32 {\n    1\n}\n\npub fn main() {\n    parse();\n}\n\n\
             #[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn parses() {\n        \
             assert_eq!(parse(), 1);\n    }\n}\n",
        )
        .unwrap();
    }

    fn edited_parse(dir: &Path) -> Vec<EditedSymbol> {
        let changed = [("src/lib.rs".to_string(), BTreeSet::from([2]))].into();
        edited_symbols(dir, &["src/lib.rs".to_string()], &changed).unwrap()
    }

    #[test]
    fn a_change_outside_an_item_or_the_diff_blocks_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let root = dir.path();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/x.md"), "x").unwrap();
        let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let lines = |l: &[u32]| {
            [("src/lib.rs".to_string(), l.iter().copied().collect())]
                .into_iter()
                .collect()
        };

        // Line 4 is the blank line between `parse` and `main`.
        let err = edited_symbols(root, &paths(&["src/lib.rs"]), &lines(&[2, 4]))
            .err()
            .unwrap();
        assert!(err.contains("line 4"), "{err}");
        // A changed file with no diff lines: new, deleted, or not Rust.
        for other in ["src/new.rs", "Cargo.toml"] {
            let err = edited_symbols(root, &paths(&["src/lib.rs", other]), &lines(&[2]))
                .err()
                .unwrap();
            assert!(err.contains(other), "{err}");
        }
        // Outside every crate: no tests of its own to miss.
        let bare = tempfile::tempdir().unwrap();
        let ok = edited_symbols(bare.path(), &paths(&["docs/x.md"]), &lines(&[]));
        assert!(ok.unwrap().is_empty());
    }

    #[tokio::test]
    async fn narrowing_reads_staged_and_untracked_changes_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        project(root);
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let lib = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();

        // A staged edit to the blank line between items is still seen.
        std::fs::write(
            root.join("src/lib.rs"),
            lib.replacen("}\n\n", "}\n// x\n", 1),
        )
        .unwrap();
        git(&["add", "-A"]);
        let (_, outcome) = narrow_to_tests(root, &["foo".into()]).await;
        let err = outcome.err().unwrap();
        assert!(err.contains("line 4"), "{err}");

        // An untracked file has no diff lines to map.
        git(&["reset", "-q", "--hard"]);
        std::fs::write(root.join("src/new.rs"), "fn new() {}\n").unwrap();
        let (_, outcome) = narrow_to_tests(root, &["foo".into()]).await;
        let err = outcome.err().unwrap();
        assert!(err.contains("src/new.rs"), "{err}");
    }

    #[test]
    fn whole_word_matching() {
        assert_eq!(find_word("    foo();", "foo"), Some(4));
        assert!(find_word("foobar();", "foo").is_none());
        assert!(find_word("let foo_bar = 1;", "foo").is_none());
        assert_eq!(find_word("foobar(foo)", "foo"), Some(7));
        assert_eq!(find_word("xé é", "é"), Some(4));
    }

    #[test]
    fn edited_symbols_locate_the_name() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let edited = edited_parse(dir.path());
        assert_eq!(edited.len(), 1);
        assert_eq!(edited[0].name, "parse");
        assert_eq!((edited[0].pos.line, edited[0].pos.character), (0, 7));
    }

    #[test]
    fn tree_sitter_tier_maps_mentions_in_test_code_to_their_test() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let root = dir.path();
        let sites = name_sites(root, &BTreeSet::from(["parse".to_string()]));
        // Definition, the use in `main`, and the use in the test.
        assert_eq!(sites.len(), 3);
        let (fns, crates) = test_fns_at(root, &sites);
        assert_eq!(fns, BTreeSet::from(["parses".to_string()]));
        assert_eq!(crates, BTreeSet::from(["foo".to_string()]));
    }

    #[test]
    fn picks_listed_tests_by_function_name() {
        let fns = BTreeSet::from(["parses".to_string()]);
        let listed = vec![
            "tests::parses".to_string(),
            "tests::parses_twice".to_string(),
            "src/lib.rs - parses (line 3)".to_string(),
            "parses".to_string(),
        ];
        assert_eq!(pick_tests(listed, &fns), vec!["tests::parses", "parses"]);
    }

    #[test]
    fn test_code_is_under_tests_or_below_cfg_test() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {}\n";
        assert!(!in_test_code(Path::new("src/lib.rs"), text, 1));
        assert!(in_test_code(Path::new("src/lib.rs"), text, 3));
        assert!(in_test_code(Path::new("tests/it.rs"), "fn a() {}", 1));
        assert!(in_test_code(Path::new("src/tests.rs"), "fn a() {}", 1));
    }

    #[tokio::test]
    async fn lsp_tier_uses_the_servers_references() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let root = dir.path();
        let edited = edited_parse(root);
        let client = fake_server(root, |method, params, root| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            "textDocument/references" => {
                assert_eq!(params["context"]["includeDeclaration"], true);
                assert_eq!(params["position"], json!({ "line": 0, "character": 7 }));
                let uri = wingman_lsp::client::path_to_uri(&root.join("src/lib.rs"));
                let at = |line: u32| json!({ "uri": uri, "range": {
                    "start": { "line": line, "character": 8 }, "end": { "line": line, "character": 13 } } });
                // Declaration, `main`'s call, the test's call, and a site
                // outside the project.
                let outside = wingman_lsp::client::path_to_uri(&std::env::temp_dir().join("x.rs"));
                Ok(json!([at(0), at(5), at(14),
                    { "uri": outside, "range": { "start": { "line": 0, "character": 0 },
                      "end": { "line": 0, "character": 1 } } }]))
            }
            other => Err(format!("method not found: {other}")),
        })
        .await;
        let sites = lsp_reference_sites(&[(client, &edited[0])]).await.unwrap();
        assert_eq!(sites.len(), 4);
        let (fns, _) = test_fns_at(root, &sites);
        assert_eq!(fns, BTreeSet::from(["parses".to_string()]));
    }

    #[tokio::test]
    async fn a_declining_or_empty_server_gives_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        let edited = edited_parse(dir.path());
        let declines = fake_server(dir.path(), |method, _, _| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            other => Err(format!("method not found: {other}")),
        })
        .await;
        assert!(lsp_reference_sites(&[(declines, &edited[0])])
            .await
            .is_none());
        // Still indexing: not even the declaration comes back.
        let cold = fake_server(dir.path(), |method, _, _| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            _ => Ok(json!([])),
        })
        .await;
        assert!(lsp_reference_sites(&[(cold, &edited[0])]).await.is_none());
    }

    #[tokio::test]
    async fn defined_symbols_reads_the_tree_sitter_index() {
        let dir = tempfile::tempdir().unwrap();
        project(dir.path());
        std::fs::write(dir.path().join("src/paths.rs"), "use std::path::PathBuf;\n").unwrap();
        // Defined, defined, and imported from a dependency.
        let names = BTreeSet::from([
            "parse".to_string(),
            "parses".to_string(),
            "PathBuf".to_string(),
        ]);
        let (found, via) = defined_symbols(dir.path(), &names).await;
        assert_eq!(found, names.into_iter().collect());
        assert_eq!(via, "tree-sitter index");
    }

    #[tokio::test]
    async fn workspace_symbol_resolves_exact_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let client = fake_server(dir.path(), |method, params, _| match method {
            "initialize" => Ok(json!({ "capabilities": {} })),
            "workspace/symbol" => match params["query"].as_str() {
                // Fuzzy: a near name is not the name.
                Some("Router") => Ok(json!([{ "name": "RouterConfig" }])),
                Some("expand") => Ok(json!([{ "name": "expand", "kind": 12 }])),
                _ => Err("declined".into()),
            },
            other => Err(format!("method not found: {other}")),
        })
        .await;
        let (router, expand, other) = (
            "Router".to_string(),
            "expand".to_string(),
            "Gone".to_string(),
        );
        let found = workspace_matches(&[client], &[&router, &other, &expand]).await;
        assert_eq!(found, HashSet::from([expand]));
    }
}
