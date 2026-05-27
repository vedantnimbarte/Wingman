//! File chunker.
//!
//! M3 ships a simple line-window chunker — 200 lines per chunk with 20 lines
//! of overlap. Tree-sitter-aware boundary detection (so chunks land on
//! function/class edges) is a deferred enhancement; for typical source files
//! the line-window approach already gives the model enough context to find
//! the right symbol with a follow-up `read_file`.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub path: String, // relative to project root, forward slashes
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Chunker {
    pub window_lines: u32,
    pub overlap_lines: u32,
}

impl Default for Chunker {
    fn default() -> Self {
        Self {
            window_lines: 200,
            overlap_lines: 20,
        }
    }
}

impl Chunker {
    pub fn new(window_lines: u32, overlap_lines: u32) -> Self {
        Self {
            window_lines: window_lines.max(1),
            overlap_lines: overlap_lines.min(window_lines.saturating_sub(1)),
        }
    }

    /// Chunk a file's contents. `rel_path` is stored on every chunk; pass a
    /// project-relative POSIX path.
    pub fn chunk(&self, rel_path: &str, content: &str) -> Vec<Chunk> {
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Vec::new();
        }
        let win = self.window_lines as usize;
        let stride = (win - self.overlap_lines as usize).max(1);
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < lines.len() {
            let end = (start + win).min(lines.len());
            let body = lines[start..end].join("\n");
            chunks.push(Chunk {
                path: rel_path.to_string(),
                start_line: (start + 1) as u32,
                end_line: end as u32,
                content: body,
            });
            if end == lines.len() {
                break;
            }
            start += stride;
        }
        chunks
    }
}

/// True for files we want to embed. Skips binaries (NUL byte heuristic) and
/// huge files (> 2 MB).
pub fn is_indexable_file(path: &Path, bytes: &[u8]) -> bool {
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    if bytes.len() > MAX_BYTES {
        return false;
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return false;
    }
    // Heuristic on extension as a final sanity check — keep code/docs.
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "rs" | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "c"
            | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "rb"
            | "php"
            | "scala"
            | "lua"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "psm1"
            | "sql"
            | "html"
            | "css"
            | "scss"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "md"
            | "mdx"
            | "rst"
            | "txt"
            | ""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_short_file_into_one_block() {
        let c = Chunker::new(200, 20);
        let chunks = c.chunk("foo.rs", "one\ntwo\nthree");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
    }

    #[test]
    fn chunks_long_file_with_overlap() {
        let c = Chunker::new(10, 2);
        let body: String = (1..=25).map(|i| format!("line {i}\n")).collect();
        let chunks = c.chunk("x.txt", &body);
        // stride = 8; windows: [1..10], [9..18], [17..25]
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[1].start_line, 9);
        assert_eq!(chunks[2].start_line, 17);
    }
}
