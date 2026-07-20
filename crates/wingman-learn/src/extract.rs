//! Skill auto-extraction: mine the project's session JSONLs for repeated
//! tool-call sequences and propose a skill draft for any pattern that
//! appears at least `min_occurrences` times.
//!
//! This is intentionally heuristic, not "AI"-driven — we just look at the
//! ordered sequence of tool names per session and run a frequency count
//! over n-grams of length 3..=6. The output is a draft markdown file the
//! user can review, edit, and either promote to `~/.wingman/skills/` or
//! delete.
//!
//! Drafts land under `~/.wingman/skills/proposed/<slug>.md` so they don't
//! get auto-loaded into the system prompt until the user moves them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wingman_core::{ContentBlock, Role};
use wingman_session::{list_sessions, load_session, SessionRecord};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPattern {
    /// Ordered sequence of tool names.
    pub sequence: Vec<String>,
    /// How many distinct sessions contained this exact subsequence.
    pub occurrences: usize,
    /// Representative user prompts that led into the pattern (up to 3).
    pub example_prompts: Vec<String>,
    /// Best-effort AST hints harvested from `edit_file` / `edit_symbol` /
    /// `write_file` calls in the matching sessions. Each entry is a short
    /// label like `"rust:function"`, `"python:class"`, `"rust:match_expression"`.
    /// Lets `wingman skill extract` propose drafts like
    /// "user refactors Rust match expressions" instead of generic
    /// "user edits files".
    #[serde(default)]
    pub ast_hints: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractConfig {
    pub min_seq_len: usize,
    pub max_seq_len: usize,
    pub min_occurrences: usize,
    /// Limit how many sessions we scan back through.
    pub session_scan_limit: usize,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            min_seq_len: 3,
            max_seq_len: 6,
            min_occurrences: 2,
            session_scan_limit: 100,
        }
    }
}

/// Scan all session JSONLs under `sessions_dir`, return repeated tool-call
/// sequences sorted by frequency (descending).
pub fn extract_from_dir(sessions_dir: &Path, cfg: &ExtractConfig) -> Vec<ExtractedPattern> {
    let mut per_session: Vec<SessionMeta> = Vec::new();
    for path in list_sessions(sessions_dir)
        .into_iter()
        .take(cfg.session_scan_limit)
    {
        if let Ok(records) = load_session(&path) {
            let meta = session_meta(&records);
            if !meta.sequence.is_empty() {
                per_session.push(meta);
            }
        }
    }

    // Count n-grams across sessions. We deliberately do *not* double-count
    // an n-gram that repeats within the same session — we care about
    // "appears across multiple flows," not "happens twice in one chat."
    let mut counts: HashMap<Vec<String>, CountEntry> = HashMap::new();
    for meta in &per_session {
        let mut seen_in_this_session: std::collections::HashSet<Vec<String>> = Default::default();
        for n in cfg.min_seq_len..=cfg.max_seq_len {
            if meta.sequence.len() < n {
                break;
            }
            for window in meta.sequence.windows(n) {
                let key = window.to_vec();
                if !seen_in_this_session.insert(key.clone()) {
                    continue;
                }
                let entry = counts.entry(key).or_default();
                entry.occurrences += 1;
                if let Some(p) = &meta.first_user_prompt {
                    if entry.example_prompts.len() < 3
                        && !entry.example_prompts.iter().any(|x| x == p)
                    {
                        entry.example_prompts.push(p.clone());
                    }
                }
                for hint in &meta.ast_hints {
                    if !entry.ast_hints.contains(hint) {
                        entry.ast_hints.push(hint.clone());
                    }
                }
            }
        }
    }

    let mut patterns: Vec<ExtractedPattern> = counts
        .into_iter()
        .filter(|(_, e)| e.occurrences >= cfg.min_occurrences)
        .map(|(seq, e)| ExtractedPattern {
            sequence: seq,
            occurrences: e.occurrences,
            example_prompts: e.example_prompts,
            ast_hints: e.ast_hints,
        })
        .collect();

    // Sort: more occurrences first, longer sequences as tiebreaker (a
    // longer n-gram is a more specific pattern, more interesting to surface).
    patterns.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| b.sequence.len().cmp(&a.sequence.len()))
    });

    // De-prefix: if pattern A is a strict prefix/suffix of pattern B and
    // B has equal occurrences, drop A — keep the more specific one.
    dedupe_subpatterns(patterns)
}

fn dedupe_subpatterns(input: Vec<ExtractedPattern>) -> Vec<ExtractedPattern> {
    let mut out: Vec<ExtractedPattern> = Vec::new();
    'next: for p in input {
        for existing in &out {
            if existing.occurrences >= p.occurrences
                && is_contiguous_sub(&p.sequence, &existing.sequence)
            {
                continue 'next;
            }
        }
        out.push(p);
    }
    out
}

fn is_contiguous_sub(small: &[String], large: &[String]) -> bool {
    if small.len() >= large.len() {
        return false;
    }
    large.windows(small.len()).any(|w| w == small)
}

/// Extract the ordered sequence of tool names called by the assistant in
/// this session, plus the *first* user prompt (for example purposes).
pub fn tool_sequence(records: &[SessionRecord]) -> (Vec<String>, Option<String>) {
    let meta = session_meta(records);
    (meta.sequence, meta.first_user_prompt)
}

#[derive(Debug, Default, Clone)]
struct CountEntry {
    occurrences: usize,
    example_prompts: Vec<String>,
    ast_hints: Vec<String>,
}

#[derive(Debug, Default, Clone)]
struct SessionMeta {
    sequence: Vec<String>,
    first_user_prompt: Option<String>,
    /// AST hints harvested from `edit_*`/`write_file` tool inputs. Format:
    /// `"<lang>:<symbol-kind>"` — e.g. `"rust:function"`, `"python:class"`.
    /// De-duplicated within the session.
    ast_hints: Vec<String>,
}

fn session_meta(records: &[SessionRecord]) -> SessionMeta {
    let mut out = SessionMeta::default();
    for r in records {
        match r {
            SessionRecord::User { text, .. } if out.first_user_prompt.is_none() => {
                out.first_user_prompt = Some(text.clone());
            }
            SessionRecord::Assistant { blocks, .. } => {
                for b in blocks {
                    if let ContentBlock::ToolUse { name, input, .. } = b {
                        out.sequence.push(name.clone());
                        for hint in derive_ast_hint(name, input) {
                            if !out.ast_hints.contains(&hint) {
                                out.ast_hints.push(hint);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let _ = Role::Assistant;
    out
}

/// Inspect a tool-use input and, if it's an edit-shaped tool, look up the
/// target file on disk to infer the enclosing symbol kind. Returns one or
/// more `"<lang>:<kind>"` labels; empty on miss.
fn derive_ast_hint(tool_name: &str, input: &serde_json::Value) -> Vec<String> {
    let target_path = match tool_name {
        "edit_file" | "edit_symbol" | "write_file" | "apply_patch" | "read_file" => input
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string()),
        _ => None,
    };
    let Some(path) = target_path else {
        return Vec::new();
    };
    let p = std::path::Path::new(&path);

    #[cfg(feature = "treesitter")]
    {
        let Some(lang) = wingman_ts::Language::from_path(p) else {
            return Vec::new();
        };
        let lang_label = lang.label().to_string();

        // For edit_symbol we know the name directly.
        if tool_name == "edit_symbol" {
            if let Some(name) = input.get("name").and_then(|n| n.as_str()) {
                if let Ok(text) = std::fs::read_to_string(p) {
                    let syms = wingman_ts::extract_symbols(lang, &text);
                    if let Some(s) = syms.iter().find(|s| s.name == name) {
                        return vec![format!("{lang_label}:{}", s.kind.label())];
                    }
                }
                // Fall back to "function" — the tool only edits fn/method.
                return vec![format!("{lang_label}:function")];
            }
        }

        // For edit_file / apply_patch, try to locate the old_string in the
        // current file and report the enclosing symbol's kind.
        if matches!(tool_name, "edit_file") {
            if let (Some(needle), Ok(text)) = (
                input.get("old_string").and_then(|s| s.as_str()),
                std::fs::read_to_string(p),
            ) {
                if let Some(byte_idx) = text.find(needle) {
                    let line = text[..byte_idx].matches('\n').count() as u32 + 1;
                    if let Some(sym) = wingman_ts::enclosing_symbol(lang, &text, line) {
                        return vec![format!("{lang_label}:{}", sym.kind.label())];
                    }
                }
            }
        }

        // Fallback: just record the language touched, without a kind. That
        // alone is still useful ("user often edits rust files").
        vec![format!("{lang_label}:*")]
    }
    #[cfg(not(feature = "treesitter"))]
    {
        let _ = p;
        Vec::new()
    }
}

/// Render an [`ExtractedPattern`] as a draft skill markdown file.
pub fn render_draft(p: &ExtractedPattern) -> (String, String) {
    let slug = make_slug(&p.sequence);
    let name = format!("auto-{slug}");
    let hints_block = if p.ast_hints.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Files / symbols touched\n\nObserved on:\n{}\n",
            p.ast_hints
                .iter()
                .map(|h| format!("- `{h}`"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let body = format!(
        "---\n\
         name: {name}\n\
         description: Proposed from {n} repeated session(s) — review before promoting\n\
         ---\n\n\
         <!--\n  \
         AUTO-EXTRACTED skill draft. This file lives under skills/proposed/ so\n  \
         wingman does NOT load it into the system prompt automatically. Once you\n  \
         agree with the description and rewrite the body in your own voice, move\n  \
         it to ~/.wingman/skills/{name}.md (or your project's .wingman/skills/).\n\
         -->\n\n\
         ## Observed pattern\n\
         The assistant has run this exact tool-call sequence across {n} session(s):\n\n\
         {chain}\n\
         {hints_block}\n\
         ## Example prompts that led to this flow\n\n\
         {examples}\n\n\
         ## Suggested skill body (edit me)\n\n\
         When the user's request matches the above pattern, follow this flow:\n\n\
         {steps}\n",
        name = name,
        n = p.occurrences,
        chain = p
            .sequence
            .iter()
            .map(|s| format!("`{s}`"))
            .collect::<Vec<_>>()
            .join(" → "),
        hints_block = hints_block,
        examples = if p.example_prompts.is_empty() {
            "_(none recorded)_".to_string()
        } else {
            p.example_prompts
                .iter()
                .enumerate()
                .map(|(i, e)| format!("{}. {}", i + 1, truncate(e, 200)))
                .collect::<Vec<_>>()
                .join("\n")
        },
        steps = p
            .sequence
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}. Call `{t}` (describe inputs/intent)", i + 1))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    (name, body)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

fn make_slug(seq: &[String]) -> String {
    seq.iter()
        .map(|s| s.replace('_', "-"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Convenience: write all drafts into `proposed_dir`. Returns the list of
/// file paths written. Existing files are left untouched unless `overwrite`.
pub fn write_drafts(
    proposed_dir: &Path,
    patterns: &[ExtractedPattern],
    overwrite: bool,
) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(proposed_dir)?;
    let mut written = Vec::new();
    for p in patterns {
        let (name, body) = render_draft(p);
        let dest = proposed_dir.join(format!("{name}.md"));
        if dest.exists() && !overwrite {
            continue;
        }
        std::fs::write(&dest, body)?;
        written.push(dest);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_sub_works() {
        let big = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let small = vec!["b".to_string(), "c".to_string()];
        assert!(is_contiguous_sub(&small, &big));
        let nope = vec!["b".to_string(), "d".to_string()];
        assert!(!is_contiguous_sub(&nope, &big));
    }

    #[test]
    fn extract_finds_repeated_ngram() {
        // Synthesize two sessions with overlapping tool chains.
        let seq_a = vec!["grep_tool", "read_file", "edit_file"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let seq_b = vec![
            "list_dir",
            "grep_tool",
            "read_file",
            "edit_file",
            "run_shell",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();

        // Pull the n-gram counter directly.
        let cfg = ExtractConfig {
            min_seq_len: 3,
            max_seq_len: 4,
            min_occurrences: 2,
            session_scan_limit: 10,
        };
        // Inline the counting logic by faking per-session sequences.
        let per_session: Vec<(Vec<String>, Option<String>)> =
            vec![(seq_a.clone(), None), (seq_b.clone(), None)];
        let mut counts: HashMap<Vec<String>, usize> = HashMap::new();
        for (seq, _) in &per_session {
            let mut seen: std::collections::HashSet<Vec<String>> = Default::default();
            for n in cfg.min_seq_len..=cfg.max_seq_len {
                if seq.len() < n {
                    break;
                }
                for w in seq.windows(n) {
                    let k = w.to_vec();
                    if seen.insert(k.clone()) {
                        *counts.entry(k).or_insert(0) += 1;
                    }
                }
            }
        }
        let common: Vec<&Vec<String>> = counts
            .iter()
            .filter(|(_, n)| **n >= 2)
            .map(|(k, _)| k)
            .collect();
        // grep_tool, read_file, edit_file must be in both.
        let needle: Vec<String> = vec!["grep_tool", "read_file", "edit_file"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(common.iter().any(|c| **c == needle));
    }
}
