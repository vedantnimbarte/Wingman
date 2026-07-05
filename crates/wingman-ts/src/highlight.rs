//! Tree-sitter-highlight integration for the TUI.
//!
//! Only enabled with the `highlight` feature. Returns spans of
//! `(byte_range, scope)` that the TUI can map onto its theme palette.
//!
//! Scopes follow the standard tree-sitter highlight names ("keyword",
//! "string", "function", …). The TUI maps them to colors; we deliberately
//! don't hard-code colors here so themes stay in one place.

use tree_sitter_highlight::{HighlightConfiguration, Highlighter, HtmlRenderer};

use crate::Language;

/// Names recognized in our `.scm` queries. Order matters — the index of
/// each name is what `tree-sitter-highlight` emits.
pub const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "function",
    "function.builtin",
    "function.macro",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

fn config_for(lang: Language) -> Option<HighlightConfiguration> {
    let (ts_lang, highlights, injections, locals) = match lang {
        Language::Rust => (
            tree_sitter_rust::language(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        ),
        Language::Python => (
            tree_sitter_python::language(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        Language::JavaScript => (
            tree_sitter_javascript::language(),
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        Language::TypeScript | Language::Tsx => {
            let ts = if matches!(lang, Language::Tsx) {
                tree_sitter_typescript::language_tsx()
            } else {
                tree_sitter_typescript::language_typescript()
            };
            (
                ts,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                "",
                tree_sitter_typescript::LOCALS_QUERY,
            )
        }
        Language::Go => (
            tree_sitter_go::language(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
    };
    let mut cfg =
        HighlightConfiguration::new(ts_lang, lang.label(), highlights, injections, locals).ok()?;
    cfg.configure(HIGHLIGHT_NAMES);
    Some(cfg)
}

/// A single highlight span.
#[derive(Debug, Clone)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    /// Index into [`HIGHLIGHT_NAMES`], or `None` for plain text.
    pub scope: Option<usize>,
}

/// Highlight `src` and return the resulting spans. Falls back to a
/// single plain-text span on parser failure.
pub fn highlight(lang: Language, src: &str) -> Vec<Span> {
    let Some(cfg) = config_for(lang) else {
        return vec![Span {
            start_byte: 0,
            end_byte: src.len(),
            scope: None,
        }];
    };
    let mut highlighter = Highlighter::new();
    let events = match highlighter.highlight(&cfg, src.as_bytes(), None, |_| None) {
        Ok(it) => it,
        Err(_) => {
            return vec![Span {
                start_byte: 0,
                end_byte: src.len(),
                scope: None,
            }]
        }
    };
    let mut out = Vec::new();
    let mut scope_stack: Vec<usize> = Vec::new();
    for ev in events {
        match ev {
            Ok(tree_sitter_highlight::HighlightEvent::HighlightStart(h)) => {
                scope_stack.push(h.0);
            }
            Ok(tree_sitter_highlight::HighlightEvent::HighlightEnd) => {
                scope_stack.pop();
            }
            Ok(tree_sitter_highlight::HighlightEvent::Source { start, end }) => {
                out.push(Span {
                    start_byte: start,
                    end_byte: end,
                    scope: scope_stack.last().copied(),
                });
            }
            Err(_) => break,
        }
    }
    if out.is_empty() {
        out.push(Span {
            start_byte: 0,
            end_byte: src.len(),
            scope: None,
        });
    }
    out
}

/// HTML rendering helper (kept primarily for tests and possible docs
/// output later). Inline-styles each span with `class="ts-<scope>"`.
pub fn highlight_html(lang: Language, src: &str) -> Option<String> {
    let cfg = config_for(lang)?;
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&cfg, src.as_bytes(), None, |_| None)
        .ok()?;
    let mut renderer = HtmlRenderer::new();
    renderer
        .render(events, src.as_bytes(), &|attr| {
            let class = format!("class=\"ts-{}\"", HIGHLIGHT_NAMES[attr.0]);
            // tree_sitter_highlight returns a `&[u8]` from the callback —
            // we need a stable slice, so leak a small allocation per scope.
            // In practice this fires once per scope name per call.
            Box::leak(class.into_boxed_str()).as_bytes()
        })
        .ok()?;
    Some(String::from_utf8_lossy(&renderer.html).into_owned())
}
