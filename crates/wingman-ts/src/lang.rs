use std::path::Path;

/// Set of source languages we know how to parse.
///
/// `from_path` is conservative: unknown extensions return `None` rather
/// than guessing, so callers can fall through to a line-window strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Cpp,
    Java,
    Kotlin,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Self::from_extension(&ext)
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            // `.h` stays unmapped: it is as often C as C++, and C is not a
            // language this crate parses.
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Self::Cpp,
            "java" => Self::Java,
            "kt" | "kts" => Self::Kotlin,
            _ => return None,
        })
    }

    /// A short label used in fenced code blocks and outline output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Cpp => "cpp",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_common_extensions() {
        assert_eq!(
            Language::from_path(&PathBuf::from("a.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("a.PY")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("a.tsx")),
            Some(Language::Tsx)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("a.cjs")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("a.HPP")),
            Some(Language::Cpp)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("a.java")),
            Some(Language::Java)
        );
        assert_eq!(
            Language::from_path(&PathBuf::from("build.gradle.kts")),
            Some(Language::Kotlin)
        );
        assert_eq!(Language::from_path(&PathBuf::from("a.h")), None);
        assert_eq!(Language::from_path(&PathBuf::from("a.unknown")), None);
        assert_eq!(Language::from_path(&PathBuf::from("noext")), None);
    }
}
