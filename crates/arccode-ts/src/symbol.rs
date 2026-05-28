//! Language-agnostic symbol descriptors emitted by the parser.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Interface,
    TypeAlias,
    Module,
    Constant,
    Variable,
}

impl SymbolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::TypeAlias => "type",
            Self::Module => "mod",
            Self::Constant => "const",
            Self::Variable => "let",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based, inclusive on both ends.
    pub start_line: u32,
    pub end_line: u32,
    /// Byte offsets into the original source, half-open `[start, end)`.
    pub start_byte: usize,
    pub end_byte: usize,
    /// First line of the declaration (e.g. `fn foo(x: u32) -> u32 {`).
    /// Used by the outline tool.
    pub signature: String,
}
