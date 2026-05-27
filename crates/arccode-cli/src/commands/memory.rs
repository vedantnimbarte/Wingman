//! `arccode memory …` — export, import, and diff memory packs.
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
        eprintln!("arccode: no memory directory at {}", src.display());
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
        eprintln!("arccode: pack not found: {}", src.display());
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
            "arccode: pack must be a .json file or a directory: {}",
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
    Ok(arccode_config::ensure_global_dir()?.join("memory"))
}

fn collect_pack(src: &Path) -> Result<BTreeMap<String, String>> {
    let mut pack = BTreeMap::new();
    for entry in walk_md(src) {
        let rel = entry.strip_prefix(src).unwrap_or(&entry);
        let key = rel.to_string_lossy().replace('\\', "/");
        let content = std::fs::read_to_string(&entry)
            .with_context(|| format!("read {}", entry.display()))?;
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
        if only_md
            && entry.extension().and_then(|s| s.to_str()) != Some("md")
        {
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
