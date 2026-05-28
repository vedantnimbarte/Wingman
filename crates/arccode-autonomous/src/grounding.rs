//! E2 grounding pass — produce a "facts" block from the repo.
//!
//! The planner runs this before its first LLM call so the model sees
//! real file paths + grep matches relevant to the goal, not its prior of
//! what such a project looks like. Net effect: dramatically fewer
//! hallucinated module names and untestable tasks.
//!
//! Stays in pure-Rust scope — no fastembed, no RAG, no LSP. The grounder
//! is a cheap heuristic that finds the right files most of the time;
//! deeper context (recall_session, semantic search) layers in later as
//! E6 cross-run learning matures.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Compact, prompt-ready summary of what the grounder learned about the
/// repo. Embedded into the planner's user prompt as a fenced block so
/// the model has paths + symbols to reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsBlock {
    /// Top-level directories that exist (planner can `writes:` into them).
    pub top_dirs: Vec<String>,
    /// File paths that contain any of the keywords. Capped at
    /// `MAX_HITS_PER_KEYWORD * keywords.len()` so the block doesn't
    /// dominate the context window.
    pub keyword_hits: Vec<KeywordHit>,
    /// Raw keyword list — useful when the planner wants to know what
    /// the grounder searched for.
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordHit {
    pub keyword: String,
    /// Repo-relative path of the file.
    pub path: String,
    /// First matching line (truncated).
    pub line: String,
}

/// Cap on how many hits per keyword we surface to the model.
const MAX_HITS_PER_KEYWORD: usize = 6;
/// Cap on how many files we scan before bailing — keeps grounding fast
/// on huge monorepos.
const MAX_FILES_SCANNED: usize = 5000;
/// Cap on bytes we read per file. Big files (e.g. lock files) are
/// scanned but only their head.
const MAX_BYTES_PER_FILE: usize = 256 * 1024;
/// Line truncation cap for keyword hits.
const MAX_LINE_LEN: usize = 200;

/// Tokenize the goal, lower-case, drop stopwords + words shorter than 3
/// characters. Returns up to 8 distinct keywords ordered by their
/// position in the goal (the user's first word is usually the most
/// important).
pub fn extract_keywords(goal: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for raw in goal
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|s| !s.is_empty())
    {
        let lower = raw.to_ascii_lowercase();
        // Stay verb/noun-y. Drop quintessential stopwords + arccode
        // filler. The list is short on purpose — over-pruning hurts.
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        if lower.chars().count() < 3 {
            continue;
        }
        if seen.insert(lower.clone()) {
            out.push(lower);
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "into", "that", "this", "but",
    "you", "your", "our", "are", "was", "were", "has", "have", "had",
    "will", "would", "should", "could", "can", "may", "must", "just",
    "also", "use", "using", "used", "add", "make", "build", "create",
    "new", "old", "any", "all", "some", "more", "less", "than",
    "let", "let's", "lets", "want", "need", "make", "code",
];

/// Walk the repo, grep for keyword matches, return the facts block.
/// Skips `.git`, `target`, `node_modules`, `.arccode/worktrees`, and
/// other usual-suspect bulk directories.
pub fn scan_repo_for_facts(root: &Path, keywords: &[String]) -> FactsBlock {
    let top_dirs = list_top_dirs(root);
    let mut hits_by_keyword: std::collections::HashMap<&str, Vec<KeywordHit>> =
        keywords.iter().map(|k| (k.as_str(), Vec::new())).collect();
    let mut scanned = 0usize;

    let lowered_keywords: Vec<(String, String)> = keywords
        .iter()
        .map(|k| (k.clone(), k.to_ascii_lowercase()))
        .collect();

    walk_files(root, &mut |path, rel_path| {
        if scanned >= MAX_FILES_SCANNED {
            return false;
        }
        scanned += 1;
        let Ok(body) = std::fs::read_to_string(path) else {
            return true;
        };
        let body = if body.len() > MAX_BYTES_PER_FILE {
            &body[..MAX_BYTES_PER_FILE]
        } else {
            body.as_str()
        };
        let body_lower = body.to_ascii_lowercase();
        for (orig, lower) in &lowered_keywords {
            if !body_lower.contains(lower) {
                continue;
            }
            let bucket = hits_by_keyword.entry(orig.as_str()).or_default();
            if bucket.len() >= MAX_HITS_PER_KEYWORD {
                continue;
            }
            // Find the first matching line and truncate.
            let line = body
                .lines()
                .find(|l| l.to_ascii_lowercase().contains(lower))
                .map(|l| {
                    if l.chars().count() > MAX_LINE_LEN {
                        let mut t: String = l.chars().take(MAX_LINE_LEN - 1).collect();
                        t.push('…');
                        t
                    } else {
                        l.to_string()
                    }
                })
                .unwrap_or_default();
            bucket.push(KeywordHit {
                keyword: orig.clone(),
                path: rel_path.to_string_lossy().into_owned(),
                line,
            });
        }
        true
    });

    let mut keyword_hits: Vec<KeywordHit> = lowered_keywords
        .iter()
        .flat_map(|(orig, _)| {
            hits_by_keyword
                .remove(orig.as_str())
                .unwrap_or_default()
                .into_iter()
        })
        .collect();
    // Stable order: keyword then path.
    keyword_hits.sort_by(|a, b| a.keyword.cmp(&b.keyword).then(a.path.cmp(&b.path)));

    FactsBlock {
        top_dirs,
        keyword_hits,
        keywords: keywords.to_vec(),
    }
}

fn list_top_dirs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy().into_owned();
                if !SKIP_DIRS.contains(&name.as_str()) {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".vscode",
    ".idea",
    "dist",
    "build",
];

fn walk_files(root: &Path, visit: &mut dyn FnMut(&Path, &Path) -> bool) {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name_s = name.to_string_lossy();
            // Skip junk dirs aggressively.
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if SKIP_DIRS.contains(&name_s.as_ref()) {
                    continue;
                }
                // Skip nested .arccode/worktrees so we don't recurse into
                // every per-task worker tree.
                if name_s == ".arccode" {
                    // descend into .arccode but skip its worktrees dir
                    stack.push(path);
                    continue;
                }
                if name_s == "worktrees" {
                    // The grounder doesn't care about sibling pilot runs.
                    continue;
                }
                stack.push(path);
                continue;
            }
            // Only scan likely-text files. The heuristic is
            // language-agnostic and intentionally generous; binary
            // skipping happens implicitly because read_to_string fails
            // on non-UTF8.
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if BINARY_EXTS.contains(&ext) {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let cont = visit(&path, &rel);
            if !cont {
                return;
            }
        }
    }
}

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg",
    "pdf", "zip", "gz", "tar", "tgz", "xz", "7z",
    "exe", "dll", "so", "dylib", "a", "o", "obj", "lib",
    "wasm", "class",
    "ttf", "otf", "woff", "woff2",
    "mp3", "mp4", "mov", "wav", "ogg", "flac",
    "db", "sqlite",
];

/// Render the [`FactsBlock`] as a markdown-ish block ready to inject
/// into the planner's user prompt.
pub fn render_facts(block: &FactsBlock) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "# Repo facts (scanned this run)\n");
    if !block.top_dirs.is_empty() {
        let _ = writeln!(s, "Top-level dirs: {}", block.top_dirs.join(", "));
    }
    if !block.keywords.is_empty() {
        let _ = writeln!(
            s,
            "Keywords extracted from the goal: {}",
            block.keywords.join(", ")
        );
    }
    if block.keyword_hits.is_empty() {
        let _ = writeln!(s, "\nNo direct keyword hits in the repo.");
    } else {
        let _ = writeln!(s, "\nKeyword hits (file path → first matching line):\n");
        for h in &block.keyword_hits {
            let _ = writeln!(s, "- `[{}]` `{}`", h.keyword, h.path);
            if !h.line.is_empty() {
                let _ = writeln!(s, "    {}", h.line.trim());
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extract_keywords_skips_stopwords() {
        let kws = extract_keywords("add a dark-mode toggle to the TUI");
        assert!(kws.contains(&"dark-mode".to_string()));
        assert!(kws.contains(&"toggle".to_string()));
        assert!(kws.contains(&"tui".to_string()));
        assert!(!kws.contains(&"the".to_string()));
        assert!(!kws.contains(&"add".to_string()));
    }

    #[test]
    fn extract_keywords_caps_at_eight() {
        let kws = extract_keywords(
            "one two three four five six seven eight nine ten eleven twelve",
        );
        assert!(kws.len() <= 8);
    }

    #[test]
    fn scan_returns_top_dirs() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("README.md"), b"intro").unwrap();
        let facts = scan_repo_for_facts(dir.path(), &[]);
        assert!(facts.top_dirs.contains(&"src".into()));
        assert!(facts.top_dirs.contains(&"docs".into()));
        assert!(!facts.top_dirs.contains(&".git".into()));
    }

    #[test]
    fn scan_finds_keyword_hits_in_text_files() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            b"// add --version-only flag\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            b"This project supports --version-only output.\n",
        )
        .unwrap();
        let facts = scan_repo_for_facts(dir.path(), &["version-only".to_string()]);
        assert!(!facts.keyword_hits.is_empty(), "expected keyword hits");
        let paths: Vec<&str> = facts
            .keyword_hits
            .iter()
            .map(|h| h.path.as_str())
            .collect();
        assert!(
            paths.iter().any(|p| p.contains("main.rs"))
                || paths.iter().any(|p| p.contains("README.md")),
            "expected hits in main.rs or README.md: {paths:?}"
        );
    }

    #[test]
    fn scan_skips_skip_dirs() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(
            dir.path().join("target/debug/secrets.txt"),
            b"version-only here",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), b"version-only\n").unwrap();
        let facts = scan_repo_for_facts(dir.path(), &["version-only".to_string()]);
        for h in &facts.keyword_hits {
            assert!(!h.path.contains("target/"), "should skip target: {h:?}");
        }
    }

    #[test]
    fn render_facts_includes_keywords_and_hits() {
        let block = FactsBlock {
            top_dirs: vec!["src".into(), "docs".into()],
            keyword_hits: vec![KeywordHit {
                keyword: "foo".into(),
                path: "src/main.rs".into(),
                line: "fn foo() {}".into(),
            }],
            keywords: vec!["foo".into()],
        };
        let s = render_facts(&block);
        assert!(s.contains("src, docs"));
        assert!(s.contains("foo"));
        assert!(s.contains("src/main.rs"));
        assert!(s.contains("fn foo() {}"));
    }

    #[test]
    fn render_facts_handles_empty_hits() {
        let block = FactsBlock {
            top_dirs: vec![],
            keyword_hits: vec![],
            keywords: vec!["whatever".into()],
        };
        let s = render_facts(&block);
        assert!(s.contains("No direct keyword hits"));
    }
}
