//! Symbol-aware annotation for unified diffs.
//!
//! Given a [`FileDiff`], we look up the file on disk and ask
//! `arccode_ts::enclosing_symbol` for the symbol that contains each hunk.
//! The result is a short `"fn agent_loop"` style label that makes diffs
//! readable without rebuilding any mental model of the file's structure.

use std::path::{Path, PathBuf};

use super::diff::FileDiff;

/// One annotation per file diff.
#[derive(Debug, Default, Clone)]
pub struct FileAnnotation {
    /// Same length as `file.hunks`. `None` when no symbol contains the hunk
    /// (e.g. top-of-file imports) or when tree-sitter isn't available.
    pub hunks: Vec<Option<String>>,
}

/// Returns one [`FileAnnotation`] per input file, in the same order.
///
/// `project_root` is used to resolve relative paths from the diff against
/// the working tree.
pub fn annotate_all(project_root: &Path, files: &[FileDiff]) -> Vec<FileAnnotation> {
    files.iter().map(|f| annotate(project_root, f)).collect()
}

pub fn annotate(project_root: &Path, fd: &FileDiff) -> FileAnnotation {
    #[cfg(feature = "treesitter")]
    {
        let target = pick_target_path(project_root, fd);
        let Some(target) = target else {
            return FileAnnotation { hunks: vec![None; fd.hunks.len()] };
        };
        let Some(lang) = arccode_ts::Language::from_path(&target) else {
            return FileAnnotation { hunks: vec![None; fd.hunks.len()] };
        };
        let Ok(content) = std::fs::read_to_string(&target) else {
            return FileAnnotation { hunks: vec![None; fd.hunks.len()] };
        };
        let hunks = fd
            .hunks
            .iter()
            .map(|h| {
                // Pick the middle of the new-side range as the probe line;
                // edge cases (zero-length adds) fall back to new_start.
                let probe = if h.new_len == 0 {
                    h.new_start.max(1) as u32
                } else {
                    (h.new_start + h.new_len / 2).max(1) as u32
                };
                arccode_ts::enclosing_symbol(lang, &content, probe)
                    .map(|s| format!("{} {}", s.kind.label(), s.name))
            })
            .collect();
        FileAnnotation { hunks }
    }
    #[cfg(not(feature = "treesitter"))]
    {
        let _ = project_root;
        FileAnnotation { hunks: vec![None; fd.hunks.len()] }
    }
}

fn pick_target_path(root: &Path, fd: &FileDiff) -> Option<PathBuf> {
    let candidate = if fd.new_path.is_empty() || fd.new_path == "/dev/null" {
        &fd.old_path
    } else {
        &fd.new_path
    };
    if candidate.is_empty() || candidate == "/dev/null" {
        return None;
    }
    let abs = root.join(candidate);
    if abs.exists() {
        Some(abs)
    } else {
        Some(PathBuf::from(candidate))
    }
}

/// Format a unified-diff string with `// fn foo` markers after every
/// `@@` hunk header, so an LLM reviewer sees the enclosing symbol next
/// to each hunk.
pub fn annotate_diff_text(project_root: &Path, diff_text: &str) -> String {
    let files = super::diff::parse_unified_diff(diff_text);
    if files.is_empty() {
        return diff_text.to_string();
    }
    let annotations = annotate_all(project_root, &files);

    // Walk the original text and inject "  // <symbol>" suffixes onto
    // each "@@" line. Because the parser already gives us hunks in source
    // order matching diff order, we re-walk by file boundary.
    let mut out = String::with_capacity(diff_text.len() + 256);
    let mut file_idx: isize = -1;
    let mut hunk_idx = 0usize;
    for line in diff_text.lines() {
        if line.starts_with("diff --git") {
            file_idx += 1;
            hunk_idx = 0;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if line.starts_with("@@") {
            out.push_str(line);
            if file_idx >= 0 {
                if let Some(ann) = annotations.get(file_idx as usize) {
                    if let Some(Some(sym)) = ann.hunks.get(hunk_idx) {
                        out.push_str("  // ");
                        out.push_str(sym);
                    }
                }
            }
            out.push('\n');
            hunk_idx += 1;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}
