//! Speculative pre-read: when the agent reads a file, warm the OS page cache
//! for the source files it is *likely* to touch next, so the following
//! `read_file`/`grep` hits warm cache instead of cold disk. Fire-and-forget on
//! a blocking thread — never on the request path — so it's near-free.
//!
//! Candidates are ranked by reference proximity: the project files the read
//! file imports (tree-sitter, resolved per language) come first, then its
//! same-directory siblings. Also pre-warms `git status` once.
//!
//! ponytail: forward imports only, and only project-local ones (Rust `crate`/
//! `self`/`super`, relative Python and JS/TS, Go under this `go.mod`). Reverse
//! references (who imports this file) need a repo-wide scan or the index.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Cap how many files we warm per read, and skip large files (warming a
/// generated 5 MB file wastes IO for a read that probably won't happen).
const MAX_CANDIDATES: usize = 12;
const MAX_FILE_BYTES: u64 = 512 * 1024;

fn warmed() -> &'static Mutex<HashSet<PathBuf>> {
    static S: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Files worth warming after a read of `path`: the project files it imports,
/// in source order, then its siblings. Deduplicated, never `path` itself,
/// never an import outside `root` (`../../../x` must not widen what a read
/// can touch), capped at `max`.
pub fn prefetch_candidates(path: &Path, root: &Path, max: usize) -> Vec<PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let (Some(root), Some(me)) = (canon(root), canon(path)) else {
        return sibling_candidates(path, max);
    };
    let mut seen = HashSet::from([me]);
    let imported = import_candidates(path, &root)
        .into_iter()
        .filter_map(|p| canon(&p))
        .filter(|p| p.starts_with(&root) && p.is_file());
    let siblings = sibling_candidates(path, max)
        .into_iter()
        .filter_map(|p| canon(&p));
    imported
        .chain(siblings)
        .filter(|p| seen.insert(p.clone()))
        .take(max)
        .collect()
}

/// Source-like sibling files in the same directory as `path` (excluding
/// `path`), sorted and capped at `max`. The fallback when a file imports
/// nothing we can resolve.
pub fn sibling_candidates(path: &Path, max: usize) -> Vec<PathBuf> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p != path && is_source_like(p))
        .collect();
    out.sort();
    out.truncate(max);
    out
}

#[cfg(not(feature = "treesitter"))]
fn import_candidates(_path: &Path, _root: &Path) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(feature = "treesitter")]
fn import_candidates(path: &Path, root: &Path) -> Vec<PathBuf> {
    use wingman_ts::Language;
    let Some(lang) = Language::from_path(path) else {
        return Vec::new();
    };
    if !std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_FILE_BYTES) {
        return Vec::new();
    }
    let Ok(src) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let dir = path.parent().unwrap_or(root);
    let mut out = Vec::new();
    for spec in wingman_ts::imports(lang, &src) {
        match lang {
            Language::Rust => out.extend(resolve_rust(path, &spec)),
            Language::Python => out.extend(resolve_python(dir, root, &spec)),
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                out.extend(resolve_js(dir, &spec))
            }
            Language::Go => out.extend(resolve_go(dir, &spec)),
            // `imports` does not read these yet; prefetch falls back to the
            // file's directory neighbours for them.
            Language::Cpp | Language::Java | Language::Kotlin => {}
        }
    }
    out
}

/// The file for the longest prefix of `segs` under `base` that names a module
/// (`a::b::C` is usually item `C` of module `a/b`). A `files` entry starting
/// with `.` is an extension on the module path, otherwise a file inside it.
#[cfg(feature = "treesitter")]
fn longest_module(base: &Path, segs: &[&str], files: &[&str]) -> Option<PathBuf> {
    (1..=segs.len()).rev().find_map(|n| {
        let stem = base.join(segs[..n].join("/"));
        files
            .iter()
            .map(|f| match f.strip_prefix('.') {
                Some(ext) => with_suffix(&stem, &format!(".{ext}")),
                None => stem.join(f),
            })
            .find(|p| p.is_file())
    })
}

#[cfg(feature = "treesitter")]
fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(feature = "treesitter")]
fn resolve_rust(file: &Path, spec: &str) -> Option<PathBuf> {
    let dir = file.parent()?;
    // Where this module's children live: beside a `mod.rs`/`lib.rs`/`main.rs`,
    // in a same-named directory for `foo.rs`.
    let stem = file.file_stem()?.to_str()?;
    let children = if matches!(stem, "mod" | "lib" | "main") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    let segs: Vec<&str> = spec.split("::").collect();
    let (mut base, mut rest) = match *segs.first()? {
        "crate" => {
            let manifest = dir.ancestors().find(|d| d.join("Cargo.toml").is_file())?;
            (manifest.join("src"), &segs[1..])
        }
        "self" => (children, &segs[1..]),
        "super" => (children, &segs[..]),
        // Another crate or std: not a file in this tree we can name.
        _ => return None,
    };
    while rest.first() == Some(&"super") {
        base = base.parent()?.to_path_buf();
        rest = &rest[1..];
    }
    longest_module(&base, rest, &[".rs", "mod.rs"])
}

#[cfg(feature = "treesitter")]
fn resolve_python(dir: &Path, root: &Path, spec: &str) -> Option<PathBuf> {
    let dots = spec.chars().take_while(|&c| c == '.').count();
    let segs: Vec<&str> = spec[dots..].split('.').filter(|s| !s.is_empty()).collect();
    let files = [".py", "__init__.py"];
    if dots > 0 {
        // One dot is this package, each further dot its parent.
        return longest_module(dir.ancestors().nth(dots - 1)?, &segs, &files);
    }
    // Absolute: the project root or a `src/` layout.
    [root.to_path_buf(), root.join("src")]
        .iter()
        .find_map(|base| longest_module(base, &segs, &files))
}

#[cfg(feature = "treesitter")]
fn resolve_js(dir: &Path, spec: &str) -> Option<PathBuf> {
    // Bare specifiers are packages; only relative ones are project files.
    if !spec.starts_with('.') {
        return None;
    }
    let p = dir.join(spec);
    // TS ESM writes `./x.js` for a file that is `./x.ts` on disk.
    let bare = match p.extension().and_then(|e| e.to_str()) {
        Some("js" | "jsx" | "mjs" | "cjs") => p.with_extension(""),
        _ => p.clone(),
    };
    let exts = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    std::iter::once(p.clone())
        .chain(exts.iter().map(|e| with_suffix(&bare, &format!(".{e}"))))
        .chain(exts.iter().map(|e| p.join(format!("index.{e}"))))
        .find(|c| c.is_file())
}

/// A Go import names a package directory; under this module's `go.mod` that
/// is `<module dir>/<path after the module name>`. Yields its non-test files.
#[cfg(feature = "treesitter")]
fn resolve_go(dir: &Path, spec: &str) -> Vec<PathBuf> {
    let Some(module_dir) = dir.ancestors().find(|d| d.join("go.mod").is_file()) else {
        return Vec::new();
    };
    let gomod = std::fs::read_to_string(module_dir.join("go.mod")).unwrap_or_default();
    let module = gomod
        .lines()
        .find_map(|l| l.trim().strip_prefix("module "))
        .map(str::trim);
    let Some(rel) = module.and_then(|m| spec.strip_prefix(m)?.strip_prefix('/')) else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(module_dir.join(rel)) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            name.ends_with(".go") && !name.ends_with("_test.go")
        })
        .collect();
    out.sort();
    out
}

fn is_source_like(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "py"
                | "js"
                | "ts"
                | "tsx"
                | "jsx"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "rb"
                | "toml"
                | "json"
                | "yaml"
                | "yml"
                | "md"
                | "css"
                | "html"
        )
    )
}

/// Warm the page cache for the files a read of `path` makes likely next (see
/// [`prefetch_candidates`]) on a background thread. Each file is warmed at
/// most once per process. Requires a tokio runtime (called from the async tool
/// path); a no-op if none is present.
pub fn warm_neighbours(path: PathBuf, root: PathBuf) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let cands = prefetch_candidates(&path, &root, MAX_CANDIDATES);
        // Reserve the fresh ones under the lock, read outside it.
        let to_read: Vec<PathBuf> = {
            let mut w = warmed().lock().unwrap_or_else(|e| e.into_inner());
            cands.into_iter().filter(|c| w.insert(c.clone())).collect()
        };
        for c in to_read {
            if std::fs::metadata(&c)
                .map(|m| m.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
            {
                let _ = std::fs::read(&c); // pull into page cache, discard
            }
        }
    });
}

/// Pre-warm `git status` once per process so the first status-dependent
/// operation (diff, checkpoint) doesn't pay the cold cost. No-op off-runtime.
pub fn warm_git_status_once(root: PathBuf) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _ = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_candidates_picks_source_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("a.rs"), "").unwrap();
        std::fs::write(d.join("b.rs"), "").unwrap();
        std::fs::write(d.join("notes.md"), "").unwrap();
        std::fs::write(d.join("image.png"), "").unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();

        let cands = sibling_candidates(&d.join("a.rs"), 12);
        let names: Vec<String> = cands
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"b.rs".to_string()));
        assert!(names.contains(&"notes.md".to_string()));
        assert!(!names.contains(&"a.rs".to_string())); // excludes self
        assert!(!names.contains(&"image.png".to_string())); // not source-like
        assert!(!names.contains(&"sub".to_string())); // dirs excluded
    }

    #[test]
    fn respects_max() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "").unwrap();
        }
        assert_eq!(sibling_candidates(&dir.path().join("f0.rs"), 5).len(), 5);
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(feature = "treesitter")]
    #[test]
    fn imports_rank_ahead_of_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        std::fs::create_dir_all(src.join("net")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(src.join("aaa.rs"), "").unwrap(); // sibling, sorts first
        std::fs::write(src.join("net/mod.rs"), "").unwrap();
        std::fs::write(src.join("util.rs"), "").unwrap();
        std::fs::write(src.join("lib.rs"), "use crate::util::helper;\nmod net;\n").unwrap();

        let got = names(&prefetch_candidates(&src.join("lib.rs"), root, 12));
        // Imports in source order, then the remaining siblings; `util.rs` is
        // both and listed once, and the read file itself never.
        assert_eq!(got, ["util.rs", "mod.rs", "aaa.rs"]);
    }

    #[cfg(feature = "treesitter")]
    #[test]
    fn resolves_python_js_and_go_imports_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("pkg/sub")).unwrap();
        std::fs::create_dir_all(root.join("web/lib")).unwrap();
        std::fs::create_dir_all(root.join("app/store")).unwrap();
        std::fs::write(dir.path().join("outside.ts"), "").unwrap();

        std::fs::write(root.join("pkg/models.py"), "").unwrap();
        std::fs::write(root.join("pkg/sub/__init__.py"), "").unwrap();
        std::fs::write(
            root.join("pkg/sub/view.py"),
            "from ..models import User\nimport pkg.sub\nimport requests\n",
        )
        .unwrap();
        let py = names(&prefetch_candidates(
            &root.join("pkg/sub/view.py"),
            &root,
            12,
        ));
        assert_eq!(py, ["models.py", "__init__.py"]);

        std::fs::write(root.join("web/lib/index.ts"), "").unwrap();
        std::fs::write(root.join("web/api.ts"), "").unwrap();
        std::fs::write(
            root.join("web/main.ts"),
            "import './lib';\nimport { a } from './api.js';\nimport '../../outside';\nimport 'react';\n",
        )
        .unwrap();
        // `../../outside.ts` exists but is outside the root: never warmed.
        let js = names(&prefetch_candidates(&root.join("web/main.ts"), &root, 12));
        assert_eq!(js, ["index.ts", "api.ts"]);

        std::fs::write(root.join("app/go.mod"), "module example.com/app\n").unwrap();
        std::fs::write(root.join("app/store/db.go"), "").unwrap();
        std::fs::write(root.join("app/store/db_test.go"), "").unwrap();
        std::fs::write(
            root.join("app/main.go"),
            "package main\nimport \"example.com/app/store\"\n",
        )
        .unwrap();
        let go = names(&prefetch_candidates(&root.join("app/main.go"), &root, 12));
        assert_eq!(go, ["db.go"]);
    }

    #[test]
    fn falls_back_to_siblings_without_imports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        let got = names(&prefetch_candidates(
            &dir.path().join("a.md"),
            dir.path(),
            12,
        ));
        assert_eq!(got, ["b.rs"]);
    }
}
