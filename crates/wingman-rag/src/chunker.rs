//! File chunker.
//!
//! Files in a language `wingman-ts` parses are chunked on function/class
//! edges; everything else (and anything that fails to parse) gets a
//! line-window split — 200 lines per chunk with 20 lines of overlap.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub path: String, // relative to project root, forward slashes
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    /// Optional enclosing symbol (e.g. `fn:agent_loop`, `struct:Foo`)
    /// set by the tree-sitter chunker. `None` for line-window chunks.
    pub symbol: Option<String>,
}

pub struct Chunker {
    pub window_lines: u32,
    pub overlap_lines: u32,
    /// Parse trees from the last chunking of each file, so the watcher's
    /// re-chunk after an edit reparses only what the edit touched.
    #[cfg(feature = "treesitter")]
    trees: std::sync::Mutex<wingman_ts::TreeCache>,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new(200, 20)
    }
}

impl Chunker {
    pub fn new(window_lines: u32, overlap_lines: u32) -> Self {
        Self {
            window_lines: window_lines.max(1),
            overlap_lines: overlap_lines.min(window_lines.saturating_sub(1)),
            #[cfg(feature = "treesitter")]
            trees: Default::default(),
        }
    }

    /// Chunk a file's contents. `rel_path` is stored on every chunk; pass a
    /// project-relative POSIX path.
    ///
    /// When the `treesitter` feature is on and the path matches a known
    /// language, chunks are aligned to function/struct/class boundaries.
    /// Otherwise the line-window strategy is used.
    pub fn chunk(&self, rel_path: &str, content: &str) -> Vec<Chunk> {
        #[cfg(feature = "treesitter")]
        {
            use std::path::Path;
            if let Some(lang) = wingman_ts::Language::from_path(Path::new(rel_path)) {
                // A poisoned lock only means an earlier parse panicked; the
                // cache holds no invariant that could have broken.
                let sem = self
                    .trees
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .semantic_chunks(rel_path, lang, content);
                if !sem.is_empty() {
                    return sem
                        .into_iter()
                        .map(|c| Chunk {
                            path: rel_path.to_string(),
                            start_line: c.start_line,
                            end_line: c.end_line,
                            content: c.content,
                            symbol: c.symbol.map(|s| format!("{}:{}", s.kind.label(), s.name)),
                        })
                        .collect();
                }
                // Parsing failed or produced nothing — fall through to
                // the line-window path.
            }
        }
        self.chunk_line_window(rel_path, content)
    }

    fn chunk_line_window(&self, rel_path: &str, content: &str) -> Vec<Chunk> {
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
                symbol: None,
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
            | "kts"
            | "swift"
            | "c"
            | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "hh"
            | "hxx"
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

    #[cfg(feature = "treesitter")]
    #[test]
    fn rust_source_uses_semantic_chunks() {
        let c = Chunker::default();
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let chunks = c.chunk("file.rs", src);
        // Each fn becomes its own semantic chunk with the symbol tagged.
        let named: Vec<&str> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
        assert!(named.iter().any(|s| s.starts_with("fn:alpha")));
        assert!(named.iter().any(|s| s.starts_with("fn:beta")));
        assert!(named.iter().any(|s| s.starts_with("fn:gamma")));
    }

    #[cfg(feature = "treesitter")]
    #[test]
    fn rechunking_an_edited_file_matches_a_fresh_chunker() {
        let c = Chunker::default();
        let before = "fn alpha() {}\nfn beta() { 1 }\n";
        let after = "fn alpha() {}\nfn inserted() {}\nfn beta() { 2 }\n";
        c.chunk("file.rs", before);
        // Second pass reuses the cached tree for file.rs.
        let summary = |chunks: Vec<Chunk>| -> Vec<_> {
            chunks
                .into_iter()
                .map(|c| (c.start_line, c.end_line, c.symbol, c.content))
                .collect()
        };
        assert_eq!(
            summary(c.chunk("file.rs", after)),
            summary(Chunker::default().chunk("file.rs", after))
        );
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
