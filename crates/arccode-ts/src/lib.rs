//! Tree-sitter facade for the rest of the workspace.
//!
//! The crate intentionally hides the [`tree-sitter`] crate behind a small
//! surface (a [`Language`] enum, a [`Symbol`] struct, and a handful of free
//! functions) so consumers don't take a transitive dep on every grammar
//! they don't use.
//!
//! Build it without the `treesitter` feature to get the language detection
//! and `Symbol` types only — every parsing function returns `None` or an
//! empty `Vec`, so callers can stay parser-agnostic.

mod lang;
mod symbol;

#[cfg(feature = "treesitter")]
mod parse;

#[cfg(feature = "highlight")]
pub mod highlight;

pub use lang::Language;
pub use symbol::{Symbol, SymbolKind};

#[cfg(feature = "treesitter")]
pub use parse::{
    enclosing_symbol, extract_symbols, outline, replace_function_body, semantic_chunks,
    ParserPool, SemanticChunk,
};

// Inert fallbacks when the `treesitter` feature is off. Keeps call sites
// branch-free.
#[cfg(not(feature = "treesitter"))]
mod fallback {
    use super::{Language, Symbol};

    #[derive(Debug, Clone)]
    pub struct SemanticChunk {
        pub start_line: u32,
        pub end_line: u32,
        pub start_byte: usize,
        pub end_byte: usize,
        pub symbol: Option<Symbol>,
        pub content: String,
    }

    pub fn extract_symbols(_lang: Language, _src: &str) -> Vec<Symbol> {
        Vec::new()
    }
    pub fn semantic_chunks(_lang: Language, _src: &str) -> Vec<SemanticChunk> {
        Vec::new()
    }
    pub fn outline(_lang: Language, _src: &str) -> Option<String> {
        None
    }
    pub fn enclosing_symbol(_lang: Language, _src: &str, _line: u32) -> Option<Symbol> {
        None
    }
    pub fn replace_function_body(
        _lang: Language,
        _src: &str,
        _name: &str,
        _new_body: &str,
    ) -> Option<String> {
        None
    }
}

#[cfg(not(feature = "treesitter"))]
pub use fallback::{
    enclosing_symbol, extract_symbols, outline, replace_function_body, semantic_chunks,
    SemanticChunk,
};
