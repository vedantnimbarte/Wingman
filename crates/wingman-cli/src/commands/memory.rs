//! `wingman memory …` — export, import, and diff memory packs.
//!
//! A "pack" is either:
//!   - a directory containing `MEMORY.md` and per-memory `.md` files
//!     (mirrors the on-disk layout), or
//!   - a single JSON file mapping `"<slug>.md"` → file contents, with an
//!     `"index.md"` entry holding the `MEMORY.md` body. JSON packs are easy
//!     to share as a single attachment.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub async fn export(out: String) -> Result<ExitCode> {
    let src = memory_dir()?;
    if !src.exists() {
        eprintln!("wingman: no memory directory at {}", src.display());
        return Ok(ExitCode::SUCCESS);
    }
    let dest = PathBuf::from(&out);
    let is_json = dest
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if is_json {
        let pack = collect_pack(&src)?;
        std::fs::write(&dest, serde_json::to_string_pretty(&pack)?)
            .with_context(|| format!("write {}", dest.display()))?;
        println!("exported {} entries → {}", pack.len(), dest.display());
    } else {
        std::fs::create_dir_all(&dest).with_context(|| format!("mkdir {}", dest.display()))?;
        let n = copy_dir(&src, &dest, true)?;
        println!("exported {} files → {}", n, dest.display());
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn import(path: String, force: bool) -> Result<ExitCode> {
    let src = PathBuf::from(&path);
    let dest = memory_dir()?;
    std::fs::create_dir_all(&dest).ok();
    if !src.exists() {
        eprintln!("wingman: pack not found: {}", src.display());
        return Ok(ExitCode::from(1));
    }
    let mut imported = 0usize;
    let mut skipped = 0usize;
    if src.is_file()
        && src
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("json"))
            .unwrap_or(false)
    {
        let body = std::fs::read_to_string(&src).context("read pack")?;
        let pack: BTreeMap<String, String> = serde_json::from_str(&body).context("parse pack")?;
        for (name, content) in pack {
            let dst = dest.join(&name);
            if dst.exists() && !force {
                skipped += 1;
                continue;
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&dst, content).with_context(|| format!("write {}", dst.display()))?;
            imported += 1;
        }
    } else if src.is_dir() {
        for entry in walk_md(&src) {
            let rel = entry.strip_prefix(&src).unwrap_or(&entry).to_path_buf();
            let dst = dest.join(&rel);
            if dst.exists() && !force {
                skipped += 1;
                continue;
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::copy(&entry, &dst)
                .with_context(|| format!("copy {} → {}", entry.display(), dst.display()))?;
            imported += 1;
        }
    } else {
        eprintln!(
            "wingman: pack must be a .json file or a directory: {}",
            src.display()
        );
        return Ok(ExitCode::from(1));
    }
    println!(
        "imported {imported} files (skipped {skipped} pre-existing — pass --force to overwrite)"
    );
    Ok(ExitCode::SUCCESS)
}

pub async fn diff(a: String, b: String) -> Result<ExitCode> {
    let pack_a = load_any(&PathBuf::from(&a))?;
    let pack_b = load_any(&PathBuf::from(&b))?;
    let mut names: Vec<&String> = pack_a.keys().chain(pack_b.keys()).collect();
    names.sort();
    names.dedup();
    let mut any = false;
    for n in names {
        let aa = pack_a.get(n);
        let bb = pack_b.get(n);
        match (aa, bb) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(_), None) => {
                println!("- only in {a}: {n}");
                any = true;
            }
            (None, Some(_)) => {
                println!("+ only in {b}: {n}");
                any = true;
            }
            (Some(x), Some(y)) => {
                println!("~ differs: {n}");
                for (i, line) in similar_lines(x, y).iter().enumerate().take(40) {
                    println!("    {:>3}: {line}", i + 1);
                }
                any = true;
            }
            _ => {}
        }
    }
    if !any {
        println!("(no differences)");
    }
    Ok(ExitCode::SUCCESS)
}

fn memory_dir() -> Result<PathBuf> {
    Ok(wingman_config::ensure_global_dir()?.join("memory"))
}

/// `wingman memory push` — upload this project's memory pack to the team server
/// (`[team].endpoint`). Server-backed complement to the git-based `sync`.
pub async fn push() -> Result<ExitCode> {
    let (endpoint, token) = match team_endpoint()? {
        Some(t) => t,
        None => {
            eprintln!("wingman: set [team].endpoint (and token) to use memory push/pull");
            return Ok(ExitCode::from(1));
        }
    };
    let paths = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    let pack = collect_pack(&store.project_dir()).unwrap_or_default();
    if pack.is_empty() {
        println!("(no project memories to push)");
        return Ok(ExitCode::SUCCESS);
    }
    wingman_core::ensure_tls_provider();
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{endpoint}/memory")).json(&pack);
    if let Some(tok) = &token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.context("POST /memory")?;
    if !resp.status().is_success() {
        anyhow::bail!("push failed: HTTP {}", resp.status());
    }
    println!("pushed {} memory file(s) to {endpoint}", pack.len());
    Ok(ExitCode::SUCCESS)
}

/// `wingman memory pull` — download the team memory pack and merge it into the
/// project's memories without clobbering local files, then rebuild the index.
pub async fn pull() -> Result<ExitCode> {
    let (endpoint, token) = match team_endpoint()? {
        Some(t) => t,
        None => {
            eprintln!("wingman: set [team].endpoint (and token) to use memory push/pull");
            return Ok(ExitCode::from(1));
        }
    };
    wingman_core::ensure_tls_provider();
    let client = reqwest::Client::new();
    let mut req = client.get(format!("{endpoint}/memory"));
    if let Some(tok) = &token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.context("GET /memory")?;
    if !resp.status().is_success() {
        anyhow::bail!("pull failed: HTTP {}", resp.status());
    }
    let pack: BTreeMap<String, String> = resp.json().await.context("parse memory pack")?;

    let paths = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    let dir = store.project_dir();
    std::fs::create_dir_all(&dir).ok();
    let mut added = 0usize;
    for (name, content) in pack {
        // Only the leaf filename; never clobber a local memory.
        let fname = Path::new(&name)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or(name);
        if fname == "MEMORY.md" {
            continue;
        }
        let dst = dir.join(&fname);
        if dst.exists() {
            continue;
        }
        std::fs::write(&dst, content).with_context(|| format!("write {}", dst.display()))?;
        added += 1;
    }
    let slugs = store
        .rebuild_project_index()
        .map_err(|e| anyhow::anyhow!("rebuild index: {e}"))?;
    println!(
        "pulled from {endpoint}: {added} new memory file(s); index now has {}",
        slugs.len()
    );
    Ok(ExitCode::SUCCESS)
}

/// Resolve `[team].endpoint` + token, expanding `${ENV_VAR}` in the token.
fn team_endpoint() -> Result<Option<(String, Option<String>)>> {
    let global = wingman_config::global_config_path()?;
    let project = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    let cfg = wingman_config::Config::load(Some(&global), project_file.as_deref())?;
    let Some(endpoint) = cfg.team.endpoint.filter(|e| !e.trim().is_empty()) else {
        return Ok(None);
    };
    let token = cfg.team.token.map(|t| expand_env(&t));
    Ok(Some((endpoint.trim_end_matches('/').to_string(), token)))
}

/// Expand a single `${ENV_VAR}` reference (or return the literal).
fn expand_env(s: &str) -> String {
    if let Some(inner) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        std::env::var(inner).unwrap_or_default()
    } else {
        s.to_string()
    }
}

/// `wingman memory sync [<git-ref>]` — reconcile the team-shared project
/// memory.
///
/// Teams commit `<project>/.wingman/memory/` to git so a new teammate's agent
/// starts with the team's accumulated knowledge. Two frictions make raw git
/// awkward: (1) the regenerated `MEMORY.md` index conflicts whenever two people
/// each add a memory, and (2) a teammate's memories on another branch aren't in
/// your tree yet. `sync` handles both:
///   - optional `<git-ref>` (e.g. `origin/main`): copy in any memory files
///     present at that ref but missing locally — never overwriting a local
///     file, so your own memories and edits are safe;
///   - always: rebuild `MEMORY.md` from the memory files on disk, resolving the
///     index conflict deterministically and folding every memory into the
///     prompt index.
pub async fn sync(git_ref: Option<String>) -> Result<ExitCode> {
    let paths = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    let dir = store.project_dir();
    std::fs::create_dir_all(&dir).ok();

    let before: std::collections::BTreeSet<String> =
        store.indexed_project_slugs().into_iter().collect();

    // 1. Optionally fold in teammate memory files from a git ref.
    let mut added_from_ref = 0usize;
    if let Some(gref) = git_ref.as_deref() {
        added_from_ref = import_memories_from_ref(&paths.root, &dir, gref)?;
    }

    // 2. Rebuild the index from whatever is now on disk.
    let slugs = store
        .rebuild_project_index()
        .map_err(|e| anyhow::anyhow!("rebuild index: {e}"))?;
    let after: std::collections::BTreeSet<String> = slugs.iter().cloned().collect();

    let newly_indexed: Vec<&String> = after.difference(&before).collect();
    let dropped: Vec<&String> = before.difference(&after).collect();

    if let Some(gref) = git_ref.as_deref() {
        println!("pulled {added_from_ref} new memory file(s) from {gref}");
    }
    println!(
        "indexed {} project memories → {}",
        slugs.len(),
        dir.join("MEMORY.md").display()
    );
    if !newly_indexed.is_empty() {
        println!("  + folded into the index: {}", join_slugs(&newly_indexed));
    }
    if !dropped.is_empty() {
        println!(
            "  - removed stale index entries (file no longer present): {}",
            join_slugs(&dropped)
        );
    }
    if newly_indexed.is_empty() && dropped.is_empty() && added_from_ref == 0 {
        println!("  (already in sync)");
    }
    Ok(ExitCode::SUCCESS)
}

fn join_slugs(slugs: &[&String]) -> String {
    slugs
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Copy memory `*.md` files present at `git_ref` into `dir`, skipping any that
/// already exist locally (never clobbering local memories/edits). Returns the
/// count added. Best-effort: a non-git repo or bad ref yields a clean error.
fn import_memories_from_ref(root: &Path, dir: &Path, git_ref: &str) -> Result<usize> {
    // List files under .wingman/memory at the ref.
    let rel_dir = ".wingman/memory";
    let listing = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", git_ref, "--", rel_dir])
        .current_dir(root)
        .output()
        .context("git ls-tree")?;
    if !listing.status.success() {
        anyhow::bail!(
            "git ls-tree {git_ref} failed: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        );
    }
    let mut added = 0usize;
    for path in String::from_utf8_lossy(&listing.stdout).lines() {
        let path = path.trim();
        if !path.ends_with(".md") || path.ends_with("MEMORY.md") {
            continue;
        }
        let file_name = Path::new(path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let dst = dir.join(&file_name);
        if dst.exists() {
            continue; // never clobber a local memory
        }
        let show = std::process::Command::new("git")
            .args(["show", &format!("{git_ref}:{path}")])
            .current_dir(root)
            .output()
            .context("git show")?;
        if show.status.success() {
            std::fs::write(&dst, &show.stdout)
                .with_context(|| format!("write {}", dst.display()))?;
            added += 1;
        }
    }
    Ok(added)
}

fn collect_pack(src: &Path) -> Result<BTreeMap<String, String>> {
    let mut pack = BTreeMap::new();
    for entry in walk_md(src) {
        let rel = entry.strip_prefix(src).unwrap_or(&entry);
        let key = rel.to_string_lossy().replace('\\', "/");
        let content =
            std::fs::read_to_string(&entry).with_context(|| format!("read {}", entry.display()))?;
        pack.insert(key, content);
    }
    Ok(pack)
}

fn load_any(p: &Path) -> Result<BTreeMap<String, String>> {
    if !p.exists() {
        anyhow::bail!("not found: {}", p.display());
    }
    if p.is_file()
        && p.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.eq_ignore_ascii_case("json"))
            .unwrap_or(false)
    {
        let body = std::fs::read_to_string(p)?;
        Ok(serde_json::from_str(&body)?)
    } else if p.is_dir() {
        collect_pack(p)
    } else {
        anyhow::bail!("expected a .json pack or a directory: {}", p.display())
    }
}

fn walk_md(src: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![src.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn copy_dir(src: &Path, dest: &Path, only_md: bool) -> Result<usize> {
    let mut n = 0;
    for entry in walk_md(src) {
        if only_md && entry.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = entry.strip_prefix(src).unwrap_or(&entry);
        let dst = dest.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&entry, &dst)?;
        n += 1;
    }
    Ok(n)
}

/// Minimal line diff: marks each line +/-/= based on a simple LCS-free
/// pairwise walk. Good enough to eyeball memory pack drift.
fn similar_lines(a: &str, b: &str) -> Vec<String> {
    let av: Vec<&str> = a.lines().collect();
    let bv: Vec<&str> = b.lines().collect();
    let mut out = Vec::new();
    let n = av.len().max(bv.len());
    for i in 0..n {
        match (av.get(i), bv.get(i)) {
            (Some(x), Some(y)) if x == y => out.push(format!("  {x}")),
            (Some(x), Some(y)) => {
                out.push(format!("- {x}"));
                out.push(format!("+ {y}"));
            }
            (Some(x), None) => out.push(format!("- {x}")),
            (None, Some(y)) => out.push(format!("+ {y}")),
            _ => {}
        }
    }
    out
}
