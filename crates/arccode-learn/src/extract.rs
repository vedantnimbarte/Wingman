//! Skill auto-extraction: mine the project's session JSONLs for repeated
//! tool-call sequences and propose a skill draft for any pattern that
//! appears at least `min_occurrences` times.
//!
//! This is intentionally heuristic, not "AI"-driven — we just look at the
//! ordered sequence of tool names per session and run a frequency count
//! over n-grams of length 3..=6. The output is a draft markdown file the
//! user can review, edit, and either promote to `~/.arccode/skills/` or
//! delete.
//!
//! Drafts land under `~/.arccode/skills/proposed/<slug>.md` so they don't
//! get auto-loaded into the system prompt until the user moves them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use arccode_core::{ContentBlock, Role};
use arccode_session::{list_sessions, load_session, SessionRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPattern {
    /// Ordered sequence of tool names.
    pub sequence: Vec<String>,
    /// How many distinct sessions contained this exact subsequence.
    pub occurrences: usize,
    /// Representative user prompts that led into the pattern (up to 3).
    pub example_prompts: Vec<String>,
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
    let mut per_session: Vec<(Vec<String>, Option<String>)> = Vec::new();
    for path in list_sessions(sessions_dir).into_iter().take(cfg.session_scan_limit) {
        if let Ok(records) = load_session(&path) {
            let (seq, first_user) = tool_sequence(&records);
            if !seq.is_empty() {
                per_session.push((seq, first_user));
            }
        }
    }

    // Count n-grams across sessions. We deliberately do *not* double-count
    // an n-gram that repeats within the same session — we care about
    // "appears across multiple flows," not "happens twice in one chat."
    let mut counts: HashMap<Vec<String>, (usize, Vec<String>)> = HashMap::new();
    for (seq, prompt) in &per_session {
        let mut seen_in_this_session: std::collections::HashSet<Vec<String>> = Default::default();
        for n in cfg.min_seq_len..=cfg.max_seq_len {
            if seq.len() < n {
                break;
            }
            for window in seq.windows(n) {
                let key = window.to_vec();
                if !seen_in_this_session.insert(key.clone()) {
                    continue;
                }
                let entry = counts.entry(key).or_insert((0, Vec::new()));
                entry.0 += 1;
                if let Some(p) = prompt {
                    if entry.1.len() < 3 && !entry.1.iter().any(|x| x == p) {
                        entry.1.push(p.clone());
                    }
                }
            }
        }
    }

    let mut patterns: Vec<ExtractedPattern> = counts
        .into_iter()
        .filter(|(_, (n, _))| *n >= cfg.min_occurrences)
        .map(|(seq, (n, examples))| ExtractedPattern {
            sequence: seq,
            occurrences: n,
            example_prompts: examples,
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
            if existing.occurrences >= p.occurrences && is_contiguous_sub(&p.sequence, &existing.sequence) {
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
    let mut seq = Vec::new();
    let mut first_user: Option<String> = None;
    for r in records {
        match r {
            SessionRecord::User { text, .. } => {
                if first_user.is_none() {
                    first_user = Some(text.clone());
                }
            }
            SessionRecord::Assistant { blocks, .. } => {
                for b in blocks {
                    if let ContentBlock::ToolUse { name, .. } = b {
                        seq.push(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let _ = Role::Assistant; // silence dead-code on unused import path
    (seq, first_user)
}

/// Render an [`ExtractedPattern`] as a draft skill markdown file.
pub fn render_draft(p: &ExtractedPattern) -> (String, String) {
    let slug = make_slug(&p.sequence);
    let name = format!("auto-{slug}");
    let body = format!(
        "---\n\
         name: {name}\n\
         description: Proposed from {n} repeated session(s) — review before promoting\n\
         ---\n\n\
         <!--\n  \
         AUTO-EXTRACTED skill draft. This file lives under skills/proposed/ so\n  \
         arccode does NOT load it into the system prompt automatically. Once you\n  \
         agree with the description and rewrite the body in your own voice, move\n  \
         it to ~/.arccode/skills/{name}.md (or your project's .arccode/skills/).\n\
         -->\n\n\
         ## Observed pattern\n\
         The assistant has run this exact tool-call sequence across {n} session(s):\n\n\
         {chain}\n\n\
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
        let big = vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()];
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
