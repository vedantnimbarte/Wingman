//! `@<path>` token expansion.
//!
//! Walks the user's submitted prompt for whitespace-delimited `@<path>`
//! tokens, reads each file (size-capped), and prepends an "Attached files"
//! block so the model sees the full content while the transcript shows
//! exactly what the user typed.

use std::path::Path;

/// Hard cap per attachment in bytes. Beyond this, the file is summarized
/// as a warning instead of being inlined.
const MAX_BYTES: usize = 100 * 1024;

pub struct Expansion {
    /// What to send to the model. Equal to the original prompt if there
    /// were no `@<path>` tokens.
    pub prompt: String,
    /// Human-readable notes the host should surface (e.g. as transcript
    /// `System` lines): files that were missing, too large, or unreadable.
    pub warnings: Vec<String>,
    /// Number of files successfully inlined.
    pub attached: usize,
}

/// Scan `prompt` for `@<path>` tokens and produce an expanded version
/// with file contents prepended.
pub fn expand(prompt: &str, project_root: &Path) -> Expansion {
    let paths = collect_paths(prompt);
    if paths.is_empty() {
        return Expansion {
            prompt: prompt.to_string(),
            warnings: Vec::new(),
            attached: 0,
        };
    }

    let mut blocks: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut attached = 0;
    for rel in &paths {
        let full = project_root.join(rel);
        match std::fs::metadata(&full) {
            Err(_) => {
                warnings.push(format!("@{rel}: file not found"));
                continue;
            }
            Ok(m) if m.len() as usize > MAX_BYTES => {
                warnings.push(format!(
                    "@{rel}: skipped, {} bytes exceeds {}KB cap",
                    m.len(),
                    MAX_BYTES / 1024
                ));
                continue;
            }
            Ok(_) => {}
        }
        match std::fs::read_to_string(&full) {
            Ok(contents) => {
                let lang = lang_hint(rel);
                blocks.push(format!("### {rel}\n```{lang}\n{contents}\n```\n"));
                attached += 1;
            }
            Err(e) => warnings.push(format!("@{rel}: read failed ({e})")),
        }
    }

    let expanded = if blocks.is_empty() {
        prompt.to_string()
    } else {
        format!(
            "Attached files:\n\n{}\n{prompt}",
            blocks.join("\n"),
            prompt = prompt
        )
    };
    Expansion {
        prompt: expanded,
        warnings,
        attached,
    }
}

/// Collect unique `@<path>` tokens from `prompt`, in first-occurrence order.
fn collect_paths(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = prompt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > start {
                if let Ok(s) = std::str::from_utf8(&bytes[start..j]) {
                    let path = s.trim_end_matches([',', '.', ';', ':', ')']);
                    if !path.is_empty() && !out.iter().any(|p| p == path) {
                        out.push(path.to_string());
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn lang_hint(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
    {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "rb" => "ruby",
        "sh" | "bash" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "md" => "markdown",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_paths_at_word_boundary() {
        let p = collect_paths("look at @Cargo.toml and @src/lib.rs");
        assert_eq!(p, vec!["Cargo.toml", "src/lib.rs"]);
    }

    #[test]
    fn ignores_at_mid_word() {
        let p = collect_paths("email me at foo@bar.com");
        assert!(p.is_empty());
    }

    #[test]
    fn deduplicates() {
        let p = collect_paths("@a.rs and @a.rs again");
        assert_eq!(p, vec!["a.rs"]);
    }

    #[test]
    fn strips_trailing_punctuation() {
        let p = collect_paths("see @file.rs, please");
        assert_eq!(p, vec!["file.rs"]);
    }
}
