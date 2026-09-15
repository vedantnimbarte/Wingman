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
#[derive(Debug, Clone, PartialEq)]
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
        Language::Cpp => tree_sitter_cpp::language(),
        Language::Java => tree_sitter_java::language(),
        Language::Kotlin => tree_sitter_kotlin::language(),
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
    match parse(lang, src) {
        Some((_parser, tree)) => symbols_in(lang, src, &tree),
        None => Vec::new(),
    }
}

fn symbols_in(lang: Language, src: &str, tree: &Tree) -> Vec<Symbol> {
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
        let is_container = is_container(sym.kind);
        out.push(sym);
        if !descends_into(lang, kind) {
            return;
        }
        child_in_container = child_in_container || is_container;
    }
    // Limit depth so we don't drown in noise from giant files. Deep enough
    // for a C++ header: include guard > namespace > namespace > template >
    // class > method is nine levels of nodes.
    if depth > 10 {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_symbols(lang, src, child, out, depth + 1, child_in_container);
    }
}

/// Kinds whose body can hold further symbols. A C++ struct has methods as
/// a class does, and a Java or Kotlin enum can declare functions; in the
/// other languages nothing is ever nested inside a struct or enum.
fn is_container(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Impl
            | SymbolKind::Trait
            | SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Module
            | SymbolKind::Struct
            | SymbolKind::Enum
    )
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
        Language::Cpp => matches!(
            kind,
            "namespace_definition" | "class_specifier" | "struct_specifier" | "union_specifier"
        ),
        Language::Java => matches!(
            kind,
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ),
        Language::Kotlin => matches!(kind, "class_declaration" | "object_declaration"),
    }
}

fn symbol_from_node(lang: Language, src: &str, node: &Node) -> Option<Symbol> {
    let (name, kind) = match lang {
        Language::Rust => rust_symbol(src, node)?,
        Language::Python => python_symbol(src, node)?,
        Language::JavaScript | Language::TypeScript | Language::Tsx => js_symbol(src, node)?,
        Language::Go => go_symbol(src, node)?,
        Language::Cpp => cpp_symbol(src, node)?,
        Language::Java => java_symbol(src, node)?,
        Language::Kotlin => kotlin_symbol(src, node)?,
    };
    // A C++ template's `template <typename T>` line belongs to the item it
    // introduces, so its chunk carries it instead of a chunk of its own.
    let span = match node.parent() {
        Some(p) if lang == Language::Cpp && p.kind() == "template_declaration" => p,
        _ => *node,
    };
    let start = span.start_position();
    let end = span.end_position();
    let signature = first_line_of(src, signature_start(lang, node), node.end_byte());
    Some(Symbol {
        name,
        kind,
        start_line: (start.row + 1) as u32,
        end_line: (end.row + 1) as u32,
        start_byte: span.start_byte(),
        end_byte: span.end_byte(),
        signature,
    })
}

/// Where a declaration's signature begins. Java and Kotlin keep annotations
/// inside the declaration, so without skipping them every `@Override` or
/// `@Test` method would be outlined as just its annotation.
fn signature_start(lang: Language, node: &Node) -> usize {
    if !matches!(lang, Language::Java | Language::Kotlin) {
        return node.start_byte();
    }
    let Some(modifiers) = kotlin_child(node, "modifiers") else {
        return node.start_byte();
    };
    let mut cursor = modifiers.walk();
    let last_annotation = modifiers
        .children(&mut cursor)
        .filter(|m| m.kind().ends_with("annotation"))
        .last();
    last_annotation.map_or(node.start_byte(), |a| a.end_byte())
}

fn first_line_of(src: &str, start: usize, end: usize) -> String {
    let bytes = src.as_bytes();
    let end = end.min(bytes.len());
    let start = start.min(end);
    let slice = bytes[start..end].trim_ascii_start();
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

fn cpp_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let kind = match node.kind() {
        // `void Foo::bar() { ... }` defines a method outside its class.
        "function_definition"
            if cpp_innermost_declarator(node)?.kind() == "qualified_identifier" =>
        {
            SymbolKind::Method
        }
        "function_definition" => SymbolKind::Function,
        // A prototype (`int add(int, int);`, or a method declared in a class
        // body) is the only place a header names the function.
        "declaration" | "field_declaration" if is_cpp_function_declarator(node) => {
            SymbolKind::Function
        }
        "type_definition" => SymbolKind::TypeAlias,
        "namespace_definition" => SymbolKind::Module,
        "alias_declaration" => SymbolKind::TypeAlias,
        // `struct Foo x;` and `class Foo;` mention a type without defining it.
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
            if node.child_by_field_name("body").is_none() =>
        {
            return None
        }
        "class_specifier" => SymbolKind::Class,
        "struct_specifier" | "union_specifier" => SymbolKind::Struct,
        "enum_specifier" => SymbolKind::Enum,
        _ => return None,
    };
    let name = match node.kind() {
        "function_definition" | "declaration" | "field_declaration" | "type_definition" => {
            cpp_declarator_name(src, node)?
        }
        _ => child_text(src, node, "name")?.to_string(),
    };
    Some((name, kind))
}

/// Follow the `declarator` chain (`int *foo()` is a pointer declarator around
/// a function declarator around the identifier) down to the name node.
fn cpp_innermost_declarator<'t>(node: &Node<'t>) -> Option<Node<'t>> {
    let mut cur = cpp_next_declarator(node)?;
    while let Some(next) = cpp_next_declarator(&cur) {
        cur = next;
    }
    Some(cur)
}

/// The declarator one level in. `&`, `(...)` and `[[attr]]` declarators
/// hold theirs as an unnamed child rather than in a `declarator` field.
fn cpp_next_declarator<'t>(node: &Node<'t>) -> Option<Node<'t>> {
    match node.kind() {
        "reference_declarator" | "parenthesized_declarator" | "attributed_declarator" => {
            let mut cursor = node.walk();
            let found = node.named_children(&mut cursor).find(|c| {
                !matches!(
                    c.kind(),
                    "attribute_declaration" | "ms_call_modifier" | "variadic_declarator"
                )
            });
            found
        }
        _ => node.child_by_field_name("declarator"),
    }
}

/// Whether a declaration declares a function. `int (*cb)(int);` does not:
/// its innermost function declarator wraps a parenthesized pointer, so `cb`
/// is a variable holding a function pointer.
fn is_cpp_function_declarator(node: &Node) -> bool {
    let mut innermost_fn = None;
    let mut cur = cpp_next_declarator(node);
    while let Some(d) = cur {
        if d.kind() == "function_declarator" {
            innermost_fn = Some(d);
        }
        cur = cpp_next_declarator(&d);
    }
    innermost_fn
        .and_then(|f| f.child_by_field_name("declarator"))
        .is_some_and(|d| d.kind() != "parenthesized_declarator")
}

/// The unqualified name a declarator declares: `ns::Foo::bar` -> `bar`, so
/// `find_symbol bar` matches an out-of-class definition too.
fn cpp_declarator_name(src: &str, node: &Node) -> Option<String> {
    let mut name = cpp_innermost_declarator(node)?;
    while let Some(inner) = name.child_by_field_name("name") {
        name = inner;
    }
    src.get(name.start_byte()..name.end_byte())
        .map(str::to_string)
}

fn java_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    let kind = match node.kind() {
        "method_declaration" | "constructor_declaration" => SymbolKind::Method,
        "class_declaration" | "record_declaration" => SymbolKind::Class,
        "interface_declaration" | "annotation_type_declaration" => SymbolKind::Interface,
        "enum_declaration" => SymbolKind::Enum,
        _ => return None,
    };
    Some((child_text(src, node, "name")?.to_string(), kind))
}

fn kotlin_symbol(src: &str, node: &Node) -> Option<(String, SymbolKind)> {
    // The Kotlin grammar declares no field names, so a declaration's name is
    // its first child of the identifier kind.
    let (name_kind, kind) = match node.kind() {
        "function_declaration" => ("simple_identifier", SymbolKind::Function),
        "object_declaration" => ("type_identifier", SymbolKind::Class),
        "type_alias" => ("type_identifier", SymbolKind::TypeAlias),
        "class_declaration" => {
            let kind = if kotlin_child(node, "interface").is_some() {
                SymbolKind::Interface
            } else if kotlin_child(node, "enum_class_body").is_some() {
                SymbolKind::Enum
            } else {
                SymbolKind::Class
            };
            ("type_identifier", kind)
        }
        _ => return None,
    };
    let name = kotlin_child(node, name_kind)?;
    Some((
        src.get(name.start_byte()..name.end_byte())?.to_string(),
        kind,
    ))
}

fn kotlin_child<'t>(node: &Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

// ─── Semantic chunking ──────────────────────────────────────────────────

/// Split `src` into chunks aligned with top-level items. Returns `None`
/// (via empty Vec) when parsing fails — callers should fall back to the
/// line-window chunker.
pub fn semantic_chunks(lang: Language, src: &str) -> Vec<SemanticChunk> {
    chunks_from_symbols(src, extract_symbols(lang, src))
}

fn chunks_from_symbols(src: &str, mut top: Vec<Symbol>) -> Vec<SemanticChunk> {
    if top.is_empty() {
        return Vec::new();
    }
    // Use only top-level symbols as chunk boundaries. The inner ones still
    // ride along inside their parent's chunk. Nesting is decided by span,
    // not kind: a Go method or a C++ `Foo::bar` definition is a method that
    // sits at the top level and needs a chunk of its own.
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
        // when non-trivial. Bare punctuation (the `;` after a C++ class) is
        // not worth a chunk.
        if sym.start_byte > cursor_byte {
            let gap_text =
                String::from_utf8_lossy(&bytes[cursor_byte..sym.start_byte]).into_owned();
            if gap_text.chars().any(char::is_alphanumeric) {
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
        if gap_text.chars().any(char::is_alphanumeric) {
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
        if is_container(sym.kind) {
            stack.push((sym.start_byte, sym.end_byte));
        }
    }
    Some(out)
}

// ─── Imports ────────────────────────────────────────────────────────────

/// Module paths `src` imports, in source order, spelled the way the language
/// names them: Rust `crate::a::b` (one path per `use` list item, `mod x;` as
/// `self::x`), Python `pkg.mod` / `..rel.mod` (a from-import names each item as
/// `module.item`), JS/TS the `import`/`export`/`require` specifier, Go the
/// import path. Resolving them to files is the caller's job.
pub fn imports(lang: Language, src: &str) -> Vec<String> {
    let Some((_parser, tree)) = parse(lang, src) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_imports(lang, src, tree.root_node(), &mut out, 0);
    out
}

fn walk_imports(lang: Language, src: &str, node: Node, out: &mut Vec<String>, depth: u32) {
    let text = |n: &Node| src.get(n.start_byte()..n.end_byte()).unwrap_or_default();
    let unquote = |n: &Node| {
        text(n)
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string()
    };
    let found = match (lang, node.kind()) {
        (Language::Rust, "use_declaration") => {
            if let Some(arg) = node.child_by_field_name("argument") {
                rust_use_paths(src, arg, "", out);
            }
            true
        }
        (Language::Rust, "mod_item") if node.child_by_field_name("body").is_none() => {
            if let Some(name) = child_text(src, &node, "name") {
                out.push(format!("self::{name}"));
            }
            true
        }
        (Language::Python, "import_statement") => {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let name = name.child_by_field_name("name").unwrap_or(name);
                out.push(text(&name).to_string());
            }
            true
        }
        (Language::Python, "import_from_statement") => {
            let module = child_text(src, &node, "module_name").unwrap_or_default();
            let sep = if module.ends_with('.') { "" } else { "." };
            let mut cursor = node.walk();
            let mut any = false;
            for name in node.children_by_field_name("name", &mut cursor) {
                let name = name.child_by_field_name("name").unwrap_or(name);
                out.push(format!("{module}{sep}{}", text(&name)));
                any = true;
            }
            if !any {
                out.push(module.to_string()); // `from x import *`
            }
            true
        }
        (
            Language::JavaScript | Language::TypeScript | Language::Tsx,
            "import_statement" | "export_statement",
        ) => {
            if let Some(source) = node.child_by_field_name("source") {
                out.push(unquote(&source));
            }
            // An export statement can wrap declarations holding `require`s.
            node.kind() == "import_statement"
        }
        (Language::JavaScript | Language::TypeScript | Language::Tsx, "call_expression") => {
            let callee = node.child_by_field_name("function");
            if callee.is_some_and(|c| matches!(text(&c), "require" | "import")) {
                let arg = node
                    .child_by_field_name("arguments")
                    .and_then(|a| a.named_child(0));
                if let Some(arg) = arg.filter(|a| a.kind() == "string") {
                    out.push(unquote(&arg));
                }
            }
            false
        }
        (Language::Go, "import_spec") => {
            if let Some(path) = node.child_by_field_name("path") {
                out.push(unquote(&path));
            }
            true
        }
        _ => false,
    };
    // Imports sit near the top of the tree; the cap only guards against
    // pathological nesting blowing the stack.
    if found || depth > 64 {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_imports(lang, src, child, out, depth + 1);
    }
}

/// Flatten one Rust use-tree (`a::{b, c::d as e, f::*}`) into full paths.
fn rust_use_paths(src: &str, node: Node, prefix: &str, out: &mut Vec<String>) {
    let join = |tail: &str| {
        if prefix.is_empty() {
            tail.to_string()
        } else {
            format!("{prefix}::{tail}")
        }
    };
    let text = |n: &Node| src.get(n.start_byte()..n.end_byte()).unwrap_or_default();
    match node.kind() {
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                rust_use_paths(src, path, prefix, out);
            }
        }
        "use_wildcard" => {
            if let Some(path) = node.named_child(0) {
                out.push(join(text(&path)));
            }
        }
        "scoped_use_list" => {
            let inner = node
                .child_by_field_name("path")
                .map(|p| join(text(&p)))
                .unwrap_or_else(|| prefix.to_string());
            if let Some(list) = node.child_by_field_name("list") {
                rust_use_paths(src, list, &inner, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                rust_use_paths(src, child, prefix, out);
            }
        }
        _ => out.push(join(text(&node))),
    }
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
    let mut stack: Vec<Node> = vec![root];
    while let Some(node) = stack.pop() {
        if is_function_like(lang, node.kind()) {
            if let Some(this_name) = function_name(lang, src, &node) {
                if this_name == name {
                    let body = match lang {
                        Language::Kotlin => kotlin_child(&node, "function_body"),
                        _ => node.child_by_field_name("body"),
                    };
                    // A declaration without a body (an interface or abstract
                    // method, `Foo() = default;`) is not the one to edit;
                    // keep looking for the definition with the same name.
                    if let Some(body) = body {
                        // Slice exclusive of the outer braces / Python indent.
                        return inner_body_span(lang, src, &body);
                    }
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

fn function_name(lang: Language, src: &str, node: &Node) -> Option<String> {
    match lang {
        Language::Cpp => cpp_declarator_name(src, node),
        Language::Kotlin => kotlin_symbol(src, node).map(|(name, _)| name),
        _ => child_text(src, node, "name").map(str::to_string),
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
        Language::Cpp => kind == "function_definition",
        Language::Java => matches!(kind, "method_declaration" | "constructor_declaration"),
        Language::Kotlin => kind == "function_declaration",
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
            } else if end > start && bytes[start] == b'=' {
                // A Kotlin expression body (`fun f() = expr`): keep the `=`
                // so the replacement is still a function body.
                Some((start + 1, end))
            } else {
                Some((start, end))
            }
        }
    }
}

// ─── Incremental reparse ────────────────────────────────────────────────

/// Source bytes the cache may hold across all files. The trees it keeps
/// grow with the source, so this bounds both; the least recently parsed
/// file is dropped first, and a file larger than the whole budget is
/// parsed but never kept.
const MAX_CACHED_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// Parse trees kept between re-chunks of the same file.
///
/// A file that comes back changed is not parsed from scratch: the change is
/// described to the old tree as one [`tree_sitter::InputEdit`] spanning
/// everything between the unchanged prefix and suffix, and the parser reuses
/// every subtree outside it. One bounding edit is exact for a single
/// `edit_file` replacement and still correct (only less incremental) for a
/// save that touched several places.
#[derive(Default)]
pub struct TreeCache {
    /// Least recently parsed first.
    entries: Vec<CachedTree>,
}

struct CachedTree {
    key: String,
    lang: Language,
    src: String,
    tree: Tree,
}

impl TreeCache {
    /// [`semantic_chunks`] for the file `key` (any stable per-file id, e.g.
    /// its project-relative path), reparsing incrementally from the tree
    /// cached for that key when there is one.
    pub fn semantic_chunks(&mut self, key: &str, lang: Language, src: &str) -> Vec<SemanticChunk> {
        match self.parse(key, lang, src) {
            Some(tree) => chunks_from_symbols(src, symbols_in(lang, src, &tree)),
            None => Vec::new(),
        }
    }

    fn parse(&mut self, key: &str, lang: Language, src: &str) -> Option<Tree> {
        let old = self
            .entries
            .iter()
            .position(|e| e.key == key)
            .map(|i| self.entries.remove(i))
            .filter(|e| e.lang == lang);
        let mut parser = parser_for(lang)?;
        let tree = match old {
            Some(mut old) => {
                old.tree.edit(&input_edit(&old.src, src));
                parser.parse(src, Some(&old.tree))?
            }
            None => parser.parse(src, None)?,
        };
        if src.len() <= MAX_CACHED_SOURCE_BYTES {
            self.entries.push(CachedTree {
                key: key.to_string(),
                lang,
                src: src.to_string(),
                tree: tree.clone(),
            });
            while self.entries.iter().map(|e| e.src.len()).sum::<usize>() > MAX_CACHED_SOURCE_BYTES
            {
                self.entries.remove(0);
            }
        }
        Some(tree)
    }
}

/// The single edit that turns `old` into `new`: everything between their
/// longest common prefix and longest common suffix. Both ends are kept on
/// char boundaries so the edit never splits a UTF-8 sequence.
fn input_edit(old: &str, new: &str) -> tree_sitter::InputEdit {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut start = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(start) {
        start -= 1;
    }
    let max_suffix = o.len().min(n.len()) - start;
    let mut suffix = o
        .iter()
        .rev()
        .zip(n.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    // The bytes after either end are the same suffix, so a boundary in one
    // string is a boundary in the other.
    while !old.is_char_boundary(o.len() - suffix) {
        suffix -= 1;
    }
    let (old_end, new_end) = (o.len() - suffix, n.len() - suffix);
    tree_sitter::InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point_at(old, start),
        old_end_position: point_at(old, old_end),
        new_end_position: point_at(new, new_end),
    }
}

/// Row and byte column of `byte` in `src`, as tree-sitter counts them.
fn point_at(src: &str, byte: usize) -> tree_sitter::Point {
    let before = &src.as_bytes()[..byte];
    let row = before.iter().filter(|&&b| b == b'\n').count();
    let column = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(byte, |nl| byte - nl - 1);
    tree_sitter::Point { row, column }
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

    fn kinds(lang: Language, src: &str) -> Vec<(SymbolKind, String)> {
        extract_symbols(lang, src)
            .into_iter()
            .map(|s| (s.kind, s.name))
            .collect()
    }

    fn has(syms: &[(SymbolKind, String)], kind: SymbolKind, name: &str) -> bool {
        syms.iter().any(|(k, n)| *k == kind && n == name)
    }

    const CPP_SRC: &str = "#include <vector>
namespace geo {
class Shape {
public:
  Shape();
  virtual double area() const { return 0; }
  void scale(double f);
};
struct Point { int x; int y; };
enum class Color { Red, Green };
using Id = int;
template <typename T> T biggest(T a, T b) { return a > b ? a : b; }
}
int *geo::Shape::raw() { return 0; }
static int add(int a, int b);
typedef struct { int y; } Pair;
struct Point origin;
";

    #[test]
    fn extracts_cpp_symbols() {
        let syms = kinds(Language::Cpp, CPP_SRC);
        assert!(has(&syms, SymbolKind::Module, "geo"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Class, "Shape"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "Shape"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "area"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "scale"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Struct, "Point"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Enum, "Color"), "{syms:?}");
        assert!(has(&syms, SymbolKind::TypeAlias, "Id"), "{syms:?}");
        assert!(has(&syms, SymbolKind::TypeAlias, "Pair"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "raw"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Function, "add"), "{syms:?}");
        // Inside a namespace functions are promoted like Rust `mod` items.
        assert!(has(&syms, SymbolKind::Method, "biggest"), "{syms:?}");
        // `struct Point origin;` names the type without defining it again.
        let points = syms.iter().filter(|(_, n)| n == "Point").count();
        assert_eq!(points, 1, "{syms:?}");
    }

    #[test]
    fn chunks_outlines_and_edits_cpp() {
        let chunks = semantic_chunks(Language::Cpp, CPP_SRC);
        let named: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol.as_ref().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(named, ["geo", "raw", "add", "Pair"]);
        assert!(chunks[0].symbol.is_none() && chunks[0].content.contains("#include"));

        let out = outline(Language::Cpp, CPP_SRC).unwrap();
        assert!(out.contains("2:mod:geo: namespace geo {"), "{out}");
        assert!(out.contains("\n  3:class:Shape: class Shape {"), "{out}");
        assert!(out.contains("\n    6:method:area:"), "{out}");

        let enc = enclosing_symbol(Language::Cpp, CPP_SRC, 12).unwrap();
        assert_eq!(enc.name, "biggest");

        let src = "int add(int a, int b) { return a + b; }\n";
        let edited = replace_function_body(Language::Cpp, src, "add", " return a - b; ").unwrap();
        assert_eq!(edited, "int add(int a, int b) { return a - b; }\n");
    }

    const JAVA_SRC: &str = "package demo;

import java.util.List;

public class Store extends Base {
    public Store() {}

    int count() {
        return 1;
    }

    interface Listener {
        void onChange();
    }

    enum Mode { READ, WRITE }
}

record Item(String name) {}

@interface Audited {}
";

    #[test]
    fn extracts_java_symbols() {
        let syms = kinds(Language::Java, JAVA_SRC);
        assert!(has(&syms, SymbolKind::Class, "Store"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "Store"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "count"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Interface, "Listener"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "onChange"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Enum, "Mode"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Class, "Item"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Interface, "Audited"), "{syms:?}");
    }

    #[test]
    fn chunks_outlines_and_edits_java() {
        let chunks = semantic_chunks(Language::Java, JAVA_SRC);
        let named: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol.as_ref().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(named, ["Store", "Item", "Audited"]);
        assert!(chunks[0].symbol.is_none() && chunks[0].content.contains("import"));

        let out = outline(Language::Java, JAVA_SRC).unwrap();
        assert!(
            out.contains("5:class:Store: public class Store extends Base {"),
            "{out}"
        );
        assert!(out.contains("\n  8:method:count:"), "{out}");
        assert!(out.contains("\n    13:method:onChange:"), "{out}");

        let enc = enclosing_symbol(Language::Java, JAVA_SRC, 9).unwrap();
        assert_eq!((enc.kind, enc.name.as_str()), (SymbolKind::Method, "count"));

        let edited =
            replace_function_body(Language::Java, JAVA_SRC, "count", " return 2; ").unwrap();
        assert!(edited.contains("int count() { return 2; }"), "{edited}");
    }

    const KOTLIN_SRC: &str = "package demo

import kotlin.math.max

class Store(val size: Int) : Base() {
    fun count(): Int {
        return size
    }

    companion object {
        fun empty() = Store(0)
    }
}

interface Listener {
    fun onChange()
}

enum class Mode {
    READ,
    WRITE
}

object Registry {
    fun lookup(): Int {
        return 0
    }
}

fun String.shout(): String {
    return uppercase()
}

typealias Id = Int
";

    #[test]
    fn extracts_kotlin_symbols() {
        let syms = kinds(Language::Kotlin, KOTLIN_SRC);
        assert!(has(&syms, SymbolKind::Class, "Store"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "count"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "empty"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Interface, "Listener"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "onChange"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Enum, "Mode"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Class, "Registry"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "lookup"), "{syms:?}");
        // An extension function is named after itself, not its receiver.
        assert!(has(&syms, SymbolKind::Function, "shout"), "{syms:?}");
        assert!(has(&syms, SymbolKind::TypeAlias, "Id"), "{syms:?}");
    }

    #[test]
    fn chunks_outlines_and_edits_kotlin() {
        let chunks = semantic_chunks(Language::Kotlin, KOTLIN_SRC);
        let named: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol.as_ref().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(
            named,
            ["Store", "Listener", "Mode", "Registry", "shout", "Id"]
        );
        assert!(chunks[0].symbol.is_none() && chunks[0].content.contains("import"));

        let out = outline(Language::Kotlin, KOTLIN_SRC).unwrap();
        assert!(
            out.contains("5:class:Store: class Store(val size: Int) : Base() {"),
            "{out}"
        );
        assert!(out.contains("\n  6:method:count:"), "{out}");
        assert!(out.contains("\n  25:method:lookup:"), "{out}");

        let enc = enclosing_symbol(Language::Kotlin, KOTLIN_SRC, 7).unwrap();
        assert_eq!((enc.kind, enc.name.as_str()), (SymbolKind::Method, "count"));

        let edited =
            replace_function_body(Language::Kotlin, KOTLIN_SRC, "shout", " return this ").unwrap();
        assert!(
            edited.contains("fun String.shout(): String { return this }"),
            "{edited}"
        );
        // A function declared in an enum class is one of its methods.
        let syms = kinds(Language::Kotlin, "enum class E { A; fun f() = 1 }");
        assert!(has(&syms, SymbolKind::Method, "f"), "{syms:?}");
        // An expression body keeps its `=`.
        let edited =
            replace_function_body(Language::Kotlin, KOTLIN_SRC, "empty", " Store(1)").unwrap();
        assert!(edited.contains("fun empty() = Store(1)\n"), "{edited}");
    }

    #[test]
    fn cpp_names_through_reference_and_pointer_declarators() {
        let src = "int& first() { return x; }
const Foo& Foo::get() const { return *this; }
std::string&& take() { return std::move(s); }
int (*callback)(int);
#ifndef V_H
namespace a {
namespace b {
template <class T> class V {
  T get() const { return t; }
};
}
}
#endif
struct Pt { int len() const { return 0; } };
";
        let syms = kinds(Language::Cpp, src);
        assert!(has(&syms, SymbolKind::Function, "first"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Method, "get"), "{syms:?}");
        assert!(has(&syms, SymbolKind::Function, "take"), "{syms:?}");
        // A function-pointer variable is not a function.
        assert!(
            !syms.iter().any(|(_, n)| n.contains("callback")),
            "{syms:?}"
        );
        // Guard, two namespaces and a template still reach the method.
        let out = outline(Language::Cpp, src).unwrap();
        assert!(out.contains("\n      9:method:get:"), "{out}");
        // A struct holds methods the way a class does.
        assert!(out.contains("\n14:struct:Pt:"), "{out}");
        assert!(out.contains("\n  14:method:len:"), "{out}");

        let edited = replace_function_body(Language::Cpp, src, "first", " return y; ").unwrap();
        assert!(
            edited.starts_with("int& first() { return y; }\n"),
            "{edited}"
        );
    }

    #[test]
    fn bodyless_declarations_annotations_and_templates() {
        // The edit lands on the definition, not on a declaration of the
        // same name that has no body, whichever comes first.
        let java = "interface Shape { int area(); }
class Sq implements Shape { public int area() { return 4; } }
abstract class B { abstract int area(); }
";
        let edited = replace_function_body(Language::Java, java, "area", " return 5; ").unwrap();
        assert!(
            edited.contains("public int area() { return 5; }"),
            "{edited}"
        );
        let kotlin = "class Sq : Shape { override fun area(): Int { return 4 } }
interface Shape { fun area(): Int }
";
        let edited = replace_function_body(Language::Kotlin, kotlin, "area", " return 5 ").unwrap();
        assert!(edited.contains("fun area(): Int { return 5 }"), "{edited}");
        let cpp =
            "struct A { int f() { return 1; } };\nstruct B { B() = default; int f() = delete; };\n";
        let edited = replace_function_body(Language::Cpp, cpp, "f", " return 2; ").unwrap();
        assert!(edited.contains("int f() { return 2; }"), "{edited}");

        // Annotations are skipped in the signature.
        let out = outline(
            Language::Java,
            "class T {\n    @Test\n    @DisplayName(\"x\")\n    void works() {}\n}\n",
        )
        .unwrap();
        assert!(out.contains("\n  2:method:works: void works() {}"), "{out}");
        let out = outline(Language::Kotlin, "class T {\n    @Test fun works() {}\n}\n").unwrap();
        assert!(out.contains("\n  2:method:works: fun works() {}"), "{out}");

        // A template's header rides in its item's chunk, and the `;` after a
        // class is not a chunk of its own.
        let chunks = semantic_chunks(
            Language::Cpp,
            "template <typename T>\nclass V {\n  T get() const { return t; }\n};\n",
        );
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 4));
        assert!(chunks[0].content.starts_with("template <typename T>"));
        let sym = chunks[0].symbol.as_ref().unwrap();
        assert_eq!(
            (sym.name.as_str(), sym.signature.as_str()),
            ("V", "class V {")
        );
    }

    #[test]
    fn semantic_chunks_keep_top_level_go_methods() {
        let src = "package p

func (s *S) A() {}

func (s *S) B() {}
";
        let chunks = semantic_chunks(Language::Go, src);
        let named: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.symbol.as_ref().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(named, ["A", "B"]);
    }

    /// Each step is compared with a from-scratch parse of the same text:
    /// the tree itself, not just the chunks, has to come out identical.
    fn assert_incremental_matches_full(lang: Language, steps: &[&str]) {
        let mut cache = TreeCache::default();
        for (i, src) in steps.iter().enumerate() {
            let incremental = cache.parse("file", lang, src).unwrap();
            let (_parser, full) = parse(lang, src).unwrap();
            assert_eq!(
                incremental.root_node().to_sexp(),
                full.root_node().to_sexp(),
                "step {i}: {src:?}"
            );
            assert_eq!(
                cache.semantic_chunks("file", lang, src),
                semantic_chunks(lang, src),
                "step {i}"
            );
        }
    }

    #[test]
    fn incremental_reparse_matches_full_reparse() {
        assert_incremental_matches_full(
            Language::Rust,
            &[
                "fn a() {}\nfn b() { 1 }\n",
                // Replace inside a body, as edit_file does.
                "fn a() {}\nfn b() { 1 + 2 }\n",
                // Insert a whole item between two others.
                "fn a() {}\nstruct S;\nimpl S { fn m(&self) {} }\nfn b() { 1 + 2 }\n",
                // Break the syntax, then repair it.
                "fn a() {\nstruct S;\nimpl S { fn m(&self) {} }\nfn b() { 1 + 2 }\n",
                "fn a() {}\nstruct S;\nimpl S { fn m(&self) {} }\nfn b() { 1 + 2 }\n",
                // Multi-byte text on both sides of the change.
                "fn a() { \"h\u{e9}llo\" }\nfn b() { \"\u{1f600}\" }\n",
                "fn a() { \"h\u{e8}llo\" }\nfn b() { \"\u{1f601}\" }\n",
                // Two separate changes in one save, and a delete to empty.
                "fn z() { \"h\u{e8}llo\" }\nfn b() { \"\u{1f601}\" }\nfn c() {}\n",
                "",
                "fn back() {}\n",
            ],
        );
        assert_incremental_matches_full(
            Language::Python,
            &[
                "class A:\n    def m(self):\n        return 1\n",
                "class A:\n    def m(self):\n        return 1\n\n    def n(self):\n        pass\n",
                "def f():\n    pass\nclass A:\n    def n(self):\n        pass\n",
            ],
        );
        // The new grammars carry external scanners (raw strings, Kotlin's
        // automatic semicolons), whose state an edit must not desync.
        assert_incremental_matches_full(
            Language::Cpp,
            &[
                "namespace n {\nint f() { return 1; }\n}\n",
                "namespace n {\nint f() { return R\"(x)\"; }\nclass C { void g(); };\n}\n",
                "namespace n {\nint f() { return R\"(x\n}\nclass C { void g(); };\n}\n",
            ],
        );
        assert_incremental_matches_full(
            Language::Java,
            &[
                "class A {\n    int f() { return 1; }\n}\n",
                "class A {\n    @Override\n    int f() { return 2; }\n}\n",
            ],
        );
        assert_incremental_matches_full(
            Language::Kotlin,
            &[
                "class A {\n    fun f() = 1\n}\n",
                "class A {\n    fun f() = 1\n    val s = \"\"\"$x\"\"\"\n}\nfun g() {}\n",
                "class A {\n    fun f() =\n}\nfun g() {}\n",
            ],
        );
    }

    #[test]
    fn input_edit_spans_only_the_change() {
        let edit = input_edit("ab\ncXd\nef", "ab\ncYYd\nef");
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (4, 5, 6)
        );
        assert_eq!(
            (edit.start_position.row, edit.start_position.column),
            (1, 1)
        );
        assert_eq!(
            (edit.new_end_position.row, edit.new_end_position.column),
            (1, 3)
        );
        // é (c3 a9) -> è (c3 a8) shares a lead byte; the edit must start
        // before it, not between the two bytes.
        let edit = input_edit("x\u{e9}y", "x\u{e8}y");
        assert_eq!((edit.start_byte, edit.old_end_byte), (1, 3));
        // An append overlaps prefix and suffix candidates; neither may
        // claim the same bytes twice.
        let edit = input_edit("aa", "aaa");
        assert_eq!(
            (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
            (2, 2, 3)
        );
    }

    #[test]
    fn tree_cache_keys_by_file_and_evicts_over_budget() {
        let mut cache = TreeCache::default();
        cache.parse("a.rs", Language::Rust, "fn a() {}").unwrap();
        cache.parse("b.rs", Language::Rust, "fn b() {}").unwrap();
        // Same key, different language: the old tree is not reused.
        let t = cache
            .parse("a.rs", Language::Python, "def a(): pass")
            .unwrap();
        assert_eq!(t.root_node().kind(), "module");
        assert_eq!(cache.entries.len(), 2);

        let big = "x".repeat(MAX_CACHED_SOURCE_BYTES / 2 + 1);
        cache.parse("big1", Language::Rust, &big).unwrap();
        cache.parse("big2", Language::Rust, &big).unwrap();
        let keys: Vec<_> = cache.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["big2"]);
    }

    /// Not a benchmark gate — timings on shared CI are noise — but run with
    /// `--nocapture` to see what the cache buys on a large file.
    #[test]
    fn incremental_reparse_timing() {
        let src: String = (0..3000)
            .map(|i| format!("fn f{i}(x: u32) -> u32 {{ x + {i} }}\n"))
            .collect();
        let edited = src.replacen("x + 1500 }", "x * 1500 + 1 }", 1);
        let mut cache = TreeCache::default();
        cache.parse("big.rs", Language::Rust, &src).unwrap();

        let t = std::time::Instant::now();
        let full = parse(Language::Rust, &edited).unwrap().1;
        let full_time = t.elapsed();
        let t = std::time::Instant::now();
        let incremental = cache.parse("big.rs", Language::Rust, &edited).unwrap();
        let incremental_time = t.elapsed();

        assert_eq!(
            incremental.root_node().to_sexp(),
            full.root_node().to_sexp()
        );
        eprintln!(
            "{} bytes: full reparse {full_time:?}, incremental {incremental_time:?}",
            edited.len()
        );
    }

    #[test]
    fn imports_flatten_rust_use_trees_and_mod_decls() {
        let src = r"
            use crate::a::{b, c::d as e, f::*};
            use super::g;
            mod h;
            mod inline { }
            fn x() { use std::fmt; }
        ";
        assert_eq!(
            imports(Language::Rust, src),
            [
                "crate::a::b",
                "crate::a::c::d",
                "crate::a::f",
                "super::g",
                "self::h",
                "std::fmt"
            ]
        );
    }

    #[test]
    fn imports_cover_python_js_and_go() {
        let py = "import os.path as p, json\nfrom . import sib\nfrom ..pkg.mod import thing\nfrom x import *\n";
        assert_eq!(
            imports(Language::Python, py),
            ["os.path", "json", ".sib", "..pkg.mod.thing", "x"]
        );
        let js = r#"
            import a from './a';
            export { b } from "../b";
            const c = require('./c');
        "#;
        assert_eq!(imports(Language::TypeScript, js), ["./a", "../b", "./c"]);
        let go = r#"
            package main
            import (
                "fmt"
                m "example.com/app/pkg"
            )
        "#;
        assert_eq!(imports(Language::Go, go), ["fmt", "example.com/app/pkg"]);
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
