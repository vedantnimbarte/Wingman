//! Source code rendered as styled lines, for fenced code blocks in the
//! transcript and for the file view.
//!
//! Scopes come from `wingman_ts::highlight`; how a scope looks is decided by
//! the theme's [`Syntax`] palette, so `tui.theme = "mono"` and `NO_COLOR`
//! reach code too. A language the parser does not know, or a build without
//! the `treesitter` feature, gets plain lines in `code_block`.
//!
//! There is deliberately no `diff` grammar here: a diff fence stays plain,
//! for the reason decisions/0016 gives for the panel — green and red would
//! read as passed and failed.

use std::path::Path;

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::theme::Theme;

/// Lines of a fenced code block whose info string is `label`.
pub fn fence_lines(body: &str, label: &str, th: &Theme) -> Vec<Line<'static>> {
    #[cfg(feature = "treesitter")]
    {
        use wingman_ts::Language;
        let label = label.to_ascii_lowercase();
        let lang = match label.as_str() {
            "rust" => Some(Language::Rust),
            "python" => Some(Language::Python),
            "javascript" => Some(Language::JavaScript),
            "typescript" => Some(Language::TypeScript),
            "c++" => Some(Language::Cpp),
            "kotlin" => Some(Language::Kotlin),
            other => Language::from_extension(other),
        };
        if let Some(lang) = lang {
            return highlighted(lang, body, th);
        }
    }
    let _ = label;
    plain(body, th)
}

/// Lines of the file at `path`, highlighted by its extension.
pub fn file_lines(body: &str, path: &Path, th: &Theme) -> Vec<Line<'static>> {
    #[cfg(feature = "treesitter")]
    if let Some(lang) = wingman_ts::Language::from_path(path) {
        return highlighted(lang, body, th);
    }
    let _ = path;
    plain(body, th)
}

/// `text` with tabs expanded and every other control character but line
/// breaks replaced, so file contents cannot drive the terminal (a stray
/// ESC would start an escape sequence) or knock the columns out of line.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\n' | '\r' => out.push(c),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

fn plain(body: &str, th: &Theme) -> Vec<Line<'static>> {
    body.lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(th.code_block),
            ))
        })
        .collect()
}

#[cfg(feature = "treesitter")]
fn highlighted(lang: wingman_ts::Language, body: &str, th: &Theme) -> Vec<Line<'static>> {
    use wingman_ts::highlight::{highlight, HIGHLIGHT_NAMES};
    let bytes = body.as_bytes();
    // One Vec<Span> per source line, splitting spans on '\n'. A `\r` before
    // the newline is dropped so CRLF files render like `str::lines` would.
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    for sp in highlight(lang, body) {
        let start = sp.start_byte.min(bytes.len());
        let end = sp.end_byte.min(bytes.len());
        let style = match sp.scope.and_then(|i| HIGHLIGHT_NAMES.get(i).copied()) {
            Some(name) => scope_style(th, name),
            None => Style::default().fg(th.code_block),
        };
        for (i, piece) in bytes[start..end.max(start)]
            .split(|&b| b == b'\n')
            .enumerate()
        {
            if i > 0 {
                lines.push(Vec::new());
            }
            let piece = piece.strip_suffix(b"\r").unwrap_or(piece);
            if !piece.is_empty() {
                let text = String::from_utf8_lossy(piece).into_owned();
                lines.last_mut().unwrap().push(Span::styled(text, style));
            }
        }
    }
    // A trailing newline ends the last line; it does not start another.
    if body.ends_with('\n') && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines.into_iter().map(Line::from).collect()
}

#[cfg(feature = "treesitter")]
fn scope_style(th: &Theme, name: &str) -> Style {
    use crate::theme::Syntax;
    use ratatui::style::{Color, Modifier};
    let color = match th.syntax {
        // Weight and slant only; the ink stays `code_block`.
        Syntax::Mono => {
            let base = Style::default().fg(th.code_block);
            return match name {
                "comment" => base.add_modifier(Modifier::DIM | Modifier::ITALIC),
                "keyword" => base.add_modifier(Modifier::BOLD),
                "string" | "string.special" => base.add_modifier(Modifier::ITALIC),
                _ => base,
            };
        }
        Syntax::Dark => match name {
            "comment" => Color::DarkGray,
            "string" | "string.special" => Color::Green,
            "number" | "constant" | "constant.builtin" => Color::LightMagenta,
            "keyword" => Color::Magenta,
            "function" | "function.builtin" | "function.macro" => Color::Yellow,
            "type" | "type.builtin" => Color::Cyan,
            "variable.builtin" | "variable.parameter" => Color::LightBlue,
            "property" | "attribute" => Color::LightCyan,
            "label" | "tag" => Color::LightYellow,
            "operator" | "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => {
                Color::Gray
            }
            _ => th.code_block,
        },
        // The dark palette's light and yellow hues wash out on a light
        // ground; these are its darker neighbours.
        Syntax::Light => match name {
            "comment" => Color::DarkGray,
            "string" | "string.special" => Color::Green,
            "number" | "constant" | "constant.builtin" => Color::Red,
            "keyword" => Color::Magenta,
            "function" | "function.builtin" | "function.macro" => Color::Blue,
            "type" | "type.builtin" => Color::Cyan,
            "variable.builtin" | "variable.parameter" => Color::Blue,
            "property" | "attribute" => Color::Cyan,
            "label" | "tag" => Color::Magenta,
            "operator" | "punctuation" | "punctuation.bracket" | "punctuation.delimiter" => {
                Color::Reset
            }
            _ => th.code_block,
        },
    };
    let style = Style::default().fg(color);
    if name == "keyword" {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

#[cfg(all(test, feature = "treesitter"))]
mod tests {
    use super::*;
    use crate::theme::{self, Syntax};
    use ratatui::style::{Color, Modifier};

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn fences_and_files_are_highlighted_by_language() {
        let th = theme::resolve(&Default::default(), false);
        let lines = fence_lines("fn main() {\n    let x = 1;\n}", "rs", &th);
        assert_eq!(text(&lines), ["fn main() {", "    let x = 1;", "}"]);
        let keyword = lines[0].spans.iter().find(|s| s.content == "fn").unwrap();
        assert_eq!(keyword.style.fg, Some(Color::Magenta));

        // CRLF and the trailing newline come out the way `str::lines` has them.
        let src = "class A:\r\n    pass\r\n";
        let lines = file_lines(src, Path::new("a.py"), &th);
        assert_eq!(text(&lines), ["class A:", "    pass"]);

        // Unknown languages, and `diff`, stay plain.
        let lines = fence_lines("+added\n-removed", "diff", &th);
        assert!(lines
            .iter()
            .flat_map(|l| &l.spans)
            .all(|s| s.style.fg == Some(th.code_block)));
    }

    #[test]
    fn mono_and_no_color_carry_scopes_without_hue() {
        for th in [
            theme::resolve(&Default::default(), true),
            theme::resolve(
                &wingman_config::TuiConfig {
                    theme: "mono".into(),
                    ..Default::default()
                },
                false,
            ),
        ] {
            assert_eq!(th.syntax, Syntax::Mono);
            let lines = fence_lines("// note\nfn f() { \"s\" }", "rust", &th);
            let spans: Vec<_> = lines.iter().flat_map(|l| &l.spans).collect();
            assert!(spans.iter().all(|s| s.style.fg == Some(th.code_block)));
            let find = |t: &str| spans.iter().find(|s| s.content == t).unwrap().style;
            assert!(find("fn").add_modifier.contains(Modifier::BOLD));
            assert!(find("// note").add_modifier.contains(Modifier::ITALIC));
        }
    }
}
