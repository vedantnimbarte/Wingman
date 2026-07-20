//! Which language server backs each language, and whether it's installed.
//!
//! Wingman does not bundle language servers — it drives whatever the user has
//! on `PATH`. This keeps the binary small and lets teams standardize on the
//! same servers their editors already use. `ServerSpec::detect` is how the
//! tools degrade gracefully: if `rust-analyzer` isn't installed, the LSP tools
//! say so and fall back to the tree-sitter heuristics instead of erroring.

use std::path::Path;

/// A language we know how to launch a server for. Mirrors the languages
/// `wingman-ts` parses, so LSP is a strict upgrade over the heuristic tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
}

impl Lang {
    /// Detect the language from a file extension. Returns `None` for files we
    /// don't drive a server for.
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "tsx" | "mts" | "cts" => Lang::TypeScript,
            "go" => Lang::Go,
            _ => return None,
        })
    }

    /// The LSP `languageId` string for `textDocument/didOpen`.
    pub fn language_id(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
            Lang::Go => "go",
        }
    }

    pub fn label(self) -> &'static str {
        self.language_id()
    }
}

/// How to launch the server for a language, plus fallbacks in preference order.
#[derive(Debug, Clone)]
pub struct ServerSpec {
    pub lang: Lang,
    /// Candidate `(program, args)` invocations, tried in order. The first whose
    /// program resolves on `PATH` wins. Multiple entries cover the common
    /// alternatives (e.g. pyright vs pylsp) so we work with whatever's present.
    pub candidates: Vec<(&'static str, Vec<&'static str>)>,
}

impl ServerSpec {
    pub fn for_lang(lang: Lang) -> ServerSpec {
        let candidates: Vec<(&'static str, Vec<&'static str>)> = match lang {
            Lang::Rust => vec![("rust-analyzer", vec![])],
            Lang::Python => vec![
                ("pyright-langserver", vec!["--stdio"]),
                ("pylsp", vec![]),
                ("jedi-language-server", vec![]),
            ],
            // typescript-language-server drives both JS and TS.
            Lang::JavaScript | Lang::TypeScript => {
                vec![("typescript-language-server", vec!["--stdio"])]
            }
            Lang::Go => vec![("gopls", vec![])],
        };
        ServerSpec { lang, candidates }
    }

    /// Return the first candidate whose program is resolvable on `PATH`, or
    /// `None` if the user has no server installed for this language.
    pub fn detect(&self) -> Option<(String, Vec<String>)> {
        for (prog, args) in &self.candidates {
            if which_on_path(prog).is_some() {
                return Some((
                    prog.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                ));
            }
        }
        None
    }

    /// Human-readable list of the programs we'd look for, for error messages.
    pub fn candidate_names(&self) -> String {
        self.candidates
            .iter()
            .map(|(p, _)| *p)
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

/// Minimal cross-platform `which`: is `program` resolvable on `PATH`?
/// Honors `PATHEXT` on Windows (.exe/.cmd/.bat), matching how npm-installed
/// servers like `typescript-language-server` land as `.cmd` shims.
pub fn which_on_path(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        // Exact name first (covers unix + an explicit extension on windows).
        let direct = dir.join(program);
        if direct.is_file() {
            return Some(direct);
        }
        if cfg!(windows) {
            for ext in &exts {
                let candidate = dir.join(format!("{program}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_detection_covers_common_extensions() {
        assert_eq!(Lang::from_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("x.py")), Some(Lang::Python));
        assert_eq!(Lang::from_path(Path::new("x.tsx")), Some(Lang::TypeScript));
        assert_eq!(Lang::from_path(Path::new("x.mjs")), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path(Path::new("x.go")), Some(Lang::Go));
        assert_eq!(Lang::from_path(Path::new("x.md")), None);
        assert_eq!(Lang::from_path(Path::new("noext")), None);
    }

    #[test]
    fn specs_have_candidates_and_names() {
        for lang in [Lang::Rust, Lang::Python, Lang::TypeScript, Lang::Go] {
            let spec = ServerSpec::for_lang(lang);
            assert!(!spec.candidates.is_empty());
            assert!(!spec.candidate_names().is_empty());
        }
    }

    #[test]
    fn which_finds_a_ubiquitous_program() {
        // `cargo` is on PATH in any environment that can run this test.
        assert!(which_on_path("cargo").is_some());
        assert!(which_on_path("definitely-not-a-real-program-xyz").is_none());
    }
}
