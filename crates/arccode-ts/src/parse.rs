//! Parser-backed symbol extraction, semantic chunking, outline, and
//! AST-aware function-body replacement.
//!
//! The parsing strategy here intentionally avoids hand-written `.scm`
//! query files (which would need to ship as data). Instead, we walk the
//! parse tree with `Cursor::goto_first_child` / `goto_next_sibling` and
//! pattern-match on `node.kind()` strings. The set of kinds we care about
//! is small per language and quite stable across grammar versions.

use std::sync::Mutex;

use tree_sitter::{Node, Parser, Tree};

use crate::{Language, Symbol, SymbolKind};

/// A chunk of source that lines up with one or more top-level symbols.
///
/// When a single symbol is larger than `max_chunk_lines`, the chunker
/// falls back to a line-window split of that one symbol; the resulting
/// chunks share the same `symbol` reference and the same byte span on the
/// outer item.
#[derive(Debug, Clone)]
pub struct SemanticChunk {
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub symbol: Option<Symbol>,
    pub content: String,
}

const DEFAULT_MAX_CHUNK_LINES: u32 = 400;

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Rust => tree_sitter_rust::language(),
        Language::Python => tree_sitter_python::language(),
        Language::JavaScript => tree_sitter_javascript::language(),
        Language::TypeScript => tree_sitter_typescript::language_typescript(),
        Language::Tsx => tree_sitter_typescript::language_tsx(),
        Language::Go => tree_sitter_go::language(),
    }
}

/// Build a parser configured for `lang`. Returns `None` if the grammar
/// rejects its own language (which would mean a grammar/runtime version
/// mismatch — not recoverable, but we'd rather degrade than panic).
fn parser_for(lang: Language) -> Option<Parser> {
    let mut p = Parser::new();
    p.set_language(&ts_language(lang)).ok()?;
    Some(p)
}

fn parse(lang: Language, src: &str) -> Option<(Parser, Tree)> {
    let mut parser = parser_for(lang)?;
    let tree = parser.parse(src, None)?;
    Some((parser, tree))
}

// ─── Symbol extraction ──────────────────────────────────────────────────

/// All top-level (and direct-child-of-impl) symbols in `src`.
pub fn extract_symbols(lang: Language, src: &str) -> Vec<Symbol> {
    let Some((_parser, tree)) = parse(lang, src) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_symbols(
        lang,
        src,
        tree.root_node(),
        &mut out,
        /*depth=*/ 0,
        /*in_container=*/ false,
    );
    out
}

fn walk_symbols(
    lang: Language,
    src: &str,
    node: Node,
    out: &mut Vec<Symbol>,
    depth: u32,
    in_container: bool,
) {
    // Recurse only inside containers that hold further named declarations
    // (module bodies, impl blocks, classes). For everything else we record
    // the symbol and stop descending.
    let kind = node.kind();
    let mut child_in_container = in_container;
    if let Some(mut sym) = symbol_from_node(lang, src, &node) {
        // Promote bare functions to methods when nested inside a class/impl.
        if in_container && matches!(sym.kind, SymbolKind::Function) {
            sym.kind = SymbolKind::Method;
        }
        let is_container = matches!(
            sym.kind,
            SymbolKind::Impl
                | SymbolKind::Trait
                | SymbolKind::Class
                | SymbolKind::Interface
                | SymbolKind::Module
        );
        out.push(sym);
        if !descends_into(lang, kind) {
            return;
        }
        child_in_container = child_in_container || is_container;
    }
    // Limit depth so we don't drown in noise from giant files.
    if depth > 6 {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(lang, src, child, out, depth + 1, child_in_container);
    }
}

fn descends_into(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => matches!(
            kind,
            "impl_item" | "mod_item" | "trait_item" | "source_file" | "declaration_list"
        ),
        Language::Python => matches!(
            kind,
            "class_definition" | "module" | "decorated_definition" | "block"
        ),
        Language::JavaScript | Language::TypeScript | Language::Tsx => matches!(
            kind,
            "program"
                | "class_declaration"
                | "class_body"
                | "export_statement"
                | "lexical_declaration"
                | "interface_body"
        ),
        Language::Go => matches!(kind, "source_file"),
    }
}

fn symbol_from_node(lang: Language, src: &str, node: &Node) -> Option<Symbol> {
    let (name, kind) = match lang {
        Language::Rust => rust_symbol(src, node)?,
        Language::Python => python_symbol(src, node)?,
        Language::JavaScript | Language::TypeScript | Language::Tsx => js_symbol(src, node)?,
        Language::Go => go_symbol(src, node)?,
    };
    let start = node.start_position();
    let end = node.end_position();
    let signature = first_line_of(src, node);
    Some(Symbol {
        name,
        kind,
        start_line: (start.row + 1) as u32,
        end_line: (end.row + 1) as u32,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        signature,
    })
}

fn first_line_of(src: &str, node: &Node) -> String {
    let bytes = src.as_bytes();
    let start = node.start_byte().min(bytes.len());
    let end = node.end_byte().min(bytes.len());
    let slice = &bytes[start..end];
    let line_end = slice
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..line_end])
        .trim_end()
        .to_string()
}

fn child_text<'a>(src: &'a str, node: &Node, field: &str) -> Option<&'a str> {
    let child = node.child_by_field_name(field)?;
    src.get(child.start_byte()..child.end_byte())
}

fn rust_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let kind = match node.kind() {
        "function_item" => SymbolKind::Function,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "mod_item" => SymbolKind::Module,
        "const_item" | "static_item" => SymbolKind::Constant,
        "type_item" => SymbolKind::TypeAlias,
        _ => return None,
    };
    let name = if node.kind() == "impl_item" {
        // `impl Foo for Bar { ... }` → name it after `Bar` (the `type` field).
        child_text(src, node, "type")
            .or_else(|| child_text(src, node, "trait"))
            .unwrap_or("impl")
            .to_string()
    } else {
        child_text(src, node, "name")?.to_string()
    };
    Some((name, kind))
}

fn python_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let kind = match node.kind() {
        "function_definition" => SymbolKind::Function,
        "class_definition" => SymbolKind::Class,
        _ => return None,
    };
    Some((child_text(src, node, "name")?.to_string(), kind))
}

fn js_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let (kind_label, kind) = match node.kind() {
        "function_declaration" | "generator_function_declaration" => ("name", SymbolKind::Function),
        "method_definition" => ("name", SymbolKind::Method),
        "class_declaration" => ("name", SymbolKind::Class),
        "interface_declaration" => ("name", SymbolKind::Interface),
        "type_alias_declaration" => ("name", SymbolKind::TypeAlias),
        _ => return None,
    };
    Some((child_text(src, node, kind_label)?.to_string(), kind))
}

fn go_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let kind = match node.kind() {
        "function_declaration" => SymbolKind::Function,
        "method_declaration" => SymbolKind::Method,
        "type_declaration" => SymbolKind::TypeAlias,
        _ => return None,
    };
    if matches!(node.kind(), "type_declaration") {
        // Walk to the first type_spec → name field.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "type_spec" {
                if let Some(name) = child_text(src, &child, "name") {
                    return Some((name.to_string(), kind));
                }
            }
        }
        return None;
    }
    Some((child_text(src, node, "name")?.to_string(), kind))
}

// ─── Semantic chunking ──────────────────────────────────────────────────

/// Split `src` into chunks aligned with top-level items. Returns `None`
/// (via empty Vec) when parsing fails — callers should fall back to the
/// line-window chunker.
pub fn semantic_chunks(lang: Language, src: &str) -> Vec<SemanticChunk> {
    let symbols = extract_symbols(lang, src);
    if symbols.is_empty() {
        return Vec::new();
    }
    // Use only top-level symbols (no methods inside impls/classes) as
    // chunk boundaries. The inner ones still ride along inside their
    // parent's chunk.
    let mut top: Vec<Symbol> = symbols
        .into_iter()
        .filter(|s| !matches!(s.kind, SymbolKind::Method))
        .collect();
    top.sort_by_key(|s| s.start_byte);
    // Drop nested symbols (start inside the previous symbol's span).
    let mut last_end = 0usize;
    top.retain(|s| {
        if s.start_byte >= last_end {
            last_end = s.end_byte;
            true
        } else {
            false
        }
    });

    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut cursor_byte = 0usize;
    let mut cursor_line: u32 = 1;
    for sym in &top {
        // Pre-symbol gap (use statements, comments, etc.) → its own chunk
        // when non-trivial.
        if sym.start_byte > cursor_byte {
            let gap_text =
                String::from_utf8_lossy(&bytes[cursor_byte..sym.start_byte]).into_owned();
            if !gap_text.trim_start().is_empty() {
                let gap_end_line = sym.start_line.saturating_sub(1).max(cursor_line);
                out.push(SemanticChunk {
                    start_line: cursor_line,
                    end_line: gap_end_line,
                    start_byte: cursor_byte,
                    end_byte: sym.start_byte,
                    symbol: None,
                    content: gap_text,
                });
            }
        }

        let item_text = String::from_utf8_lossy(&bytes[sym.start_byte..sym.end_byte]).into_owned();
        let item_lines = sym.end_line.saturating_sub(sym.start_line) + 1;
        if item_lines <= DEFAULT_MAX_CHUNK_LINES {
            out.push(SemanticChunk {
                start_line: sym.start_line,
                end_line: sym.end_line,
                start_byte: sym.start_byte,
                end_byte: sym.end_byte,
                symbol: Some(sym.clone()),
                content: item_text,
            });
        } else {
            // Oversized item: line-window split, still tagged with the
            // enclosing symbol.
            let win = DEFAULT_MAX_CHUNK_LINES as usize;
            let overlap = 20usize;
            let lines: Vec<&str> = item_text.lines().collect();
            let stride = win.saturating_sub(overlap).max(1);
            let mut s = 0usize;
            while s < lines.len() {
                let e = (s + win).min(lines.len());
                let body = lines[s..e].join("\n");
                let body_bytes = body.len();
                out.push(SemanticChunk {
                    start_line: sym.start_line + s as u32,
                    end_line: sym.start_line + e as u32 - 1,
                    // Approximate: byte boundaries inside the slice aren't
                    // easy to recover without re-walking. Use the outer
                    // span — these chunks are still referenceable.
                    start_byte: sym.start_byte,
                    end_byte: sym.start_byte + body_bytes,
                    symbol: Some(sym.clone()),
                    content: body,
                });
                if e == lines.len() {
                    break;
                }
                s += stride;
            }
        }

        cursor_byte = sym.end_byte;
        cursor_line = sym.end_line + 1;
    }
    // Trailing gap.
    if cursor_byte < bytes.len() {
        let gap_text = String::from_utf8_lossy(&bytes[cursor_byte..]).into_owned();
        if !gap_text.trim().is_empty() {
            let total_lines = src.lines().count() as u32;
            out.push(SemanticChunk {
                start_line: cursor_line,
                end_line: total_lines.max(cursor_line),
                start_byte: cursor_byte,
                end_byte: bytes.len(),
                symbol: None,
                content: gap_text,
            });
        }
    }
    out
}

// ─── Outline ────────────────────────────────────────────────────────────

/// Render a signatures-only outline of `src`. Each line is
/// `<line>:<kind>:<name>:<signature>` and indented inside impls/classes.
pub fn outline(lang: Language, src: &str) -> Option<String> {
    let symbols = extract_symbols(lang, src);
    if symbols.is_empty() {
        return None;
    }
    let mut out = String::new();
    // Sort by start byte to render in source order.
    let mut sorted = symbols;
    sorted.sort_by_key(|s| s.start_byte);
    // Track open-ended container spans for indent depth.
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (start_byte, end_byte)
    for sym in &sorted {
        while stack.last().is_some_and(|&(_, end)| sym.start_byte >= end) {
            stack.pop();
        }
        let indent = "  ".repeat(stack.len());
        out.push_str(&format!(
            "{indent}{line}:{kind}:{name}: {sig}\n",
            line = sym.start_line,
            kind = sym.kind.label(),
            name = sym.name,
            sig = sym.signature,
        ));
        // Treat containers (impl, class, mod, trait) as openings.
        if matches!(
            sym.kind,
            SymbolKind::Impl
                | SymbolKind::Class
                | SymbolKind::Module
                | SymbolKind::Trait
                | SymbolKind::Interface
        ) {
            stack.push((sym.start_byte, sym.end_byte));
        }
    }
    Some(out)
}

// ─── Enclosing symbol ───────────────────────────────────────────────────

/// Return the innermost named symbol that contains `line` (1-based).
pub fn enclosing_symbol(lang: Language, src: &str, line: u32) -> Option<Symbol> {
    let symbols = extract_symbols(lang, src);
    symbols
        .into_iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        // Innermost = smallest span.
        .min_by_key(|s| s.end_byte.saturating_sub(s.start_byte))
}

// ─── Function body replacement ──────────────────────────────────────────

/// Replace the body of the function named `name` (anywhere in `src`).
///
/// `new_body` should NOT include the outer braces — they are preserved
/// from the original. Returns `None` if no matching function/method is
/// found or the body span can't be located.
pub fn replace_function_body(
    lang: Language,
    src: &str,
    name: &str,
    new_body: &str,
) -> Option<String> {
    let (_parser, tree) = parse(lang, src)?;
    let (body_start, body_end) = find_body_span(lang, src, tree.root_node(), name)?;
    let mut out = String::with_capacity(src.len() + new_body.len());
    out.push_str(&src[..body_start]);
    out.push_str(new_body);
    out.push_str(&src[body_end..]);
    Some(out)
}

fn find_body_span(lang: Language, src: &str, root: Node, name: &str) -> Option<(usize, usize)> {
    let body_field = body_field_name(lang);
    let mut stack: Vec<Node> = vec![root];
    while let Some(node) = stack.pop() {
        if is_function_like(lang, node.kind()) {
            if let Some(this_name) = child_text(src, &node, "name") {
                if this_name == name {
                    let body = node.child_by_field_name(body_field)?;
                    // Slice exclusive of the outer braces / Python indent.
                    return inner_body_span(lang, src, &body);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn body_field_name(lang: Language) -> &'static str {
    match lang {
        Language::Rust | Language::Go => "body",
        Language::Python => "body",
        Language::JavaScript | Language::TypeScript | Language::Tsx => "body",
    }
}

fn is_function_like(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust => kind == "function_item",
        Language::Python => kind == "function_definition",
        Language::JavaScript | Language::TypeScript | Language::Tsx => matches!(
            kind,
            "function_declaration" | "method_definition" | "generator_function_declaration"
        ),
        Language::Go => matches!(kind, "function_declaration" | "method_declaration"),
    }
}

fn inner_body_span(lang: Language, src: &str, body: &Node) -> Option<(usize, usize)> {
    let start = body.start_byte();
    let end = body.end_byte();
    let bytes = src.as_bytes();
    match lang {
        Language::Python => {
            // Python bodies are `block` nodes; replace the full block.
            Some((start, end))
        }
        _ => {
            // Brace-delimited block. Strip the outer `{` and `}`.
            if end > start + 1 && bytes[start] == b'{' && bytes[end - 1] == b'}' {
                Some((start + 1, end - 1))
            } else {
                Some((start, end))
            }
        }
    }
}

// ─── Parser pool (cheap reuse for hot paths) ────────────────────────────

/// Process-wide cache of one parser per language. Tree-sitter parsers
/// retain internal scratch buffers, so reusing them across many small
/// parses (e.g. during an indexing pass) avoids repeated allocator churn.
pub struct ParserPool {
    pool: Mutex<Vec<(Language, Parser)>>,
}

impl ParserPool {
    pub const fn new() -> Self {
        Self {
            pool: Mutex::new(Vec::new()),
        }
    }

    /// Borrow a parser, parse, return the tree. The parser is returned to
    /// the pool when the closure exits.
    pub fn with<R>(&self, lang: Language, f: impl FnOnce(&mut Parser) -> R) -> Option<R> {
        let mut guard = self.pool.lock().ok()?;
        let mut parser = if let Some(pos) = guard.iter().position(|(l, _)| *l == lang) {
            guard.swap_remove(pos).1
        } else {
            drop(guard);
            let p = parser_for(lang)?;
            guard = self.pool.lock().ok()?;
            p
        };
        drop(guard);
        let out = f(&mut parser);
        if let Ok(mut g) = self.pool.lock() {
            if g.len() < 6 {
                g.push((lang, parser));
            }
        }
        Some(out)
    }
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_top_level() {
        let src = r#"
            pub fn add(a: u32, b: u32) -> u32 { a + b }
            struct Foo { x: u32 }
            impl Foo {
                pub fn bar(&self) -> u32 { self.x }
            }
        "#;
        let syms = extract_symbols(Language::Rust, src);
        let names: Vec<_> = syms.iter().map(|s| (s.kind, s.name.as_str())).collect();
        assert!(names
            .iter()
            .any(|(k, n)| matches!(k, SymbolKind::Function) && *n == "add"));
        assert!(names
            .iter()
            .any(|(k, n)| matches!(k, SymbolKind::Struct) && *n == "Foo"));
        assert!(names
            .iter()
            .any(|(k, n)| matches!(k, SymbolKind::Impl) && *n == "Foo"));
        assert!(names
            .iter()
            .any(|(k, n)| matches!(k, SymbolKind::Method) && *n == "bar"));
    }

    #[test]
    fn semantic_chunks_split_on_top_level_items() {
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let chunks = semantic_chunks(Language::Rust, src);
        // Three function chunks (gaps between them are inline / empty).
        let with_sym: Vec<_> = chunks.iter().filter(|c| c.symbol.is_some()).collect();
        assert_eq!(with_sym.len(), 3);
        assert_eq!(with_sym[0].symbol.as_ref().unwrap().name, "a");
    }

    #[test]
    fn outline_indents_inside_impls() {
        let src = "impl Foo { fn bar() {} fn baz() {} }\n";
        let out = outline(Language::Rust, src).unwrap();
        // The two methods should be indented under the impl line.
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("1:impl:Foo"));
        assert!(lines.iter().skip(1).all(|l| l.starts_with("  ")));
    }

    #[test]
    fn enclosing_symbol_finds_innermost() {
        let src = "impl Foo {\n  fn bar() {\n    let x = 1;\n  }\n}\n";
        let enc = enclosing_symbol(Language::Rust, src, 3).unwrap();
        assert_eq!(enc.name, "bar");
        assert!(matches!(enc.kind, SymbolKind::Method));
    }

    #[test]
    fn replaces_function_body_in_rust() {
        let src = "fn add(a: u32, b: u32) -> u32 { a + b }\n";
        let out = replace_function_body(Language::Rust, src, "add", " a - b ").unwrap();
        assert_eq!(out, "fn add(a: u32, b: u32) -> u32 { a - b }\n");
    }

    #[test]
    fn replaces_function_body_in_python() {
        let src = "def add(a, b):\n    return a + b\n";
        let out =
            replace_function_body(Language::Python, src, "add", "    return a - b\n").unwrap();
        assert!(out.contains("return a - b"));
        assert!(!out.contains("return a + b"));
    }

    #[test]
    fn parser_pool_reuses_parsers() {
        static POOL: ParserPool = ParserPool::new();
        let n1 = POOL.with(Language::Rust, |p| {
            let t = p.parse("fn a() {}", None).unwrap();
            t.root_node().named_child_count()
        });
        let n2 = POOL.with(Language::Rust, |p| {
            let t = p.parse("fn b() {}", None).unwrap();
            t.root_node().named_child_count()
        });
        assert_eq!(n1, n2);
    }
}
