//! `wingman skill extract` — auto-propose skill drafts from session history.
//! `wingman skill import/export` — interoperate with the portable `SKILL.md`
//! format used across the agent ecosystem (Claude Code, Codex, Cursor, Gemini
//! CLI, Copilot, Cline, Goose, …). Wingman stores skills as single `<name>.md`
//! files; the portable format is a per-skill directory holding a `SKILL.md`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use wingman_config::ProjectPaths;
use wingman_learn::extract::{extract_from_dir, write_drafts, ExtractConfig};

pub async fn extract(min: usize, force: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let project = ProjectPaths::discover(&cwd);
    let cfg = ExtractConfig {
        min_occurrences: min.max(2),
        ..Default::default()
    };
    let patterns = extract_from_dir(&project.sessions_dir, &cfg);
    if patterns.is_empty() {
        eprintln!(
            "wingman: no repeated tool-call patterns found (scanned {})",
            project.sessions_dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!("found {} candidate pattern(s):", patterns.len());
    for (i, p) in patterns.iter().enumerate() {
        println!(
            "{:>3}. ×{:<3}  {}",
            i + 1,
            p.occurrences,
            p.sequence.join(" → ")
        );
    }

    let proposed_dir = wingman_config::ensure_global_dir()?
        .join("skills")
        .join("proposed");
    let written = write_drafts(&proposed_dir, &patterns, force)?;
    if written.is_empty() {
        println!("\n(no drafts written — all candidates already exist; pass --force to overwrite)");
    } else {
        println!(
            "\nwrote {} draft(s) to {}",
            written.len(),
            proposed_dir.display()
        );
        println!("review and move into ~/.wingman/skills/ to promote.");
    }
    Ok(ExitCode::SUCCESS)
}

/// `wingman skill import <path>` — import portable `SKILL.md` skills.
///
/// `path` may be a single `SKILL.md`, a directory containing one, or a
/// directory of skill sub-directories (each with its own `SKILL.md`) — the
/// common ways the ecosystem ships skill bundles. Each becomes a wingman skill
/// at `~/.wingman/skills/<slug>.md` (or the project dir with `--project`).
pub async fn import(path: String, project: bool, force: bool) -> Result<ExitCode> {
    let src = PathBuf::from(&path);
    if !src.exists() {
        eprintln!("wingman: not found: {}", src.display());
        return Ok(ExitCode::from(1));
    }
    let dest_dir = if project {
        ProjectPaths::discover(&std::env::current_dir()?)
            .dir
            .join("skills")
    } else {
        wingman_config::ensure_global_dir()?.join("skills")
    };
    std::fs::create_dir_all(&dest_dir).ok();

    let skill_files = find_skill_md_files(&src);
    if skill_files.is_empty() {
        eprintln!("wingman: no SKILL.md found under {}", src.display());
        return Ok(ExitCode::from(1));
    }

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for sf in &skill_files {
        let text = std::fs::read_to_string(sf).with_context(|| format!("read {}", sf.display()))?;
        let (name, description, body) = parse_skill_md(&text, sf);
        let slug = slugify(&name);
        let dst = dest_dir.join(format!("{slug}.md"));
        if dst.exists() && !force {
            skipped += 1;
            continue;
        }
        std::fs::write(&dst, render_wingman_skill(&slug, &description, &body))
            .with_context(|| format!("write {}", dst.display()))?;
        imported += 1;
        println!("  imported {slug} ← {}", sf.display());
    }
    println!(
        "imported {imported} skill(s) into {} (skipped {skipped} pre-existing — pass --force to overwrite)",
        dest_dir.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `wingman skill export <name> <out-dir>` — write a wingman skill as a
/// portable `<out-dir>/<name>/SKILL.md` bundle usable by other agents.
pub async fn export(name: String, out_dir: String) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let skill = wingman_skills::load_all(&project.root)
        .into_iter()
        .find(|s| s.name == name);
    let Some(skill) = skill else {
        eprintln!("wingman: no skill named `{name}`");
        return Ok(ExitCode::from(1));
    };
    let bundle_dir = PathBuf::from(&out_dir).join(&skill.name);
    std::fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("mkdir {}", bundle_dir.display()))?;
    let path = bundle_dir.join("SKILL.md");
    std::fs::write(
        &path,
        render_skill_md(&skill.name, &skill.description, &skill.body),
    )
    .with_context(|| format!("write {}", path.display()))?;
    println!("exported skill `{}` → {}", skill.name, path.display());
    Ok(ExitCode::SUCCESS)
}

/// Collect SKILL.md files from a path: the file itself, `<dir>/SKILL.md`, or
/// each `<dir>/*/SKILL.md`. Case-insensitive on the filename.
fn find_skill_md_files(src: &Path) -> Vec<PathBuf> {
    let is_skill_md = |p: &Path| {
        p.file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.eq_ignore_ascii_case("SKILL.md"))
    };
    if src.is_file() {
        return if is_skill_md(src) {
            vec![src.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut out = Vec::new();
    let direct = src.join("SKILL.md");
    if direct.exists() {
        out.push(direct);
    }
    if let Ok(rd) = std::fs::read_dir(src) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let nested = p.join("SKILL.md");
                if nested.exists() {
                    out.push(nested);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Parse a `SKILL.md`: YAML-ish frontmatter (`name`, `description`) + body.
/// Falls back to the parent directory name when `name` is absent.
fn parse_skill_md(text: &str, path: &Path) -> (String, String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let body;
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            for line in front.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().trim_matches(['"', '\'']).to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches(['"', '\'']).to_string();
                }
            }
            // Body is everything after the closing `---` line.
            let after = &rest[end + 4..];
            body = after.trim_start_matches(['\r', '\n']).to_string();
        } else {
            body = text.to_string();
        }
    } else {
        body = text.to_string();
    }
    if name.is_empty() {
        name = path
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|f| f.to_str())
            .unwrap_or("imported-skill")
            .to_string();
    }
    (name, description, body)
}

fn render_wingman_skill(slug: &str, description: &str, body: &str) -> String {
    format!(
        "---\nname: {slug}\ndescription: {description}\n---\n{}\n",
        body.trim()
    )
}

fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n{}\n",
        body.trim()
    )
}

/// Lowercase, non-alphanumerics to hyphens, collapse/trim hyphens.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_md_frontmatter_and_body() {
        let text = "---\nname: Code Review\ndescription: Review a diff\n---\nYou are a reviewer.\nBe terse.\n";
        let (name, desc, body) = parse_skill_md(text, Path::new("x/SKILL.md"));
        assert_eq!(name, "Code Review");
        assert_eq!(desc, "Review a diff");
        assert!(body.starts_with("You are a reviewer."));
    }

    #[test]
    fn falls_back_to_dir_name_when_no_name() {
        let text = "no frontmatter here\njust a body\n";
        let (name, _desc, body) = parse_skill_md(text, Path::new("my-skill/SKILL.md"));
        assert_eq!(name, "my-skill");
        assert!(body.contains("just a body"));
    }

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Code Review!!"), "code-review");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn round_trips_through_export_format() {
        let md = render_skill_md("code-review", "Review a diff", "Body text here.");
        let (name, desc, body) = parse_skill_md(&md, Path::new("code-review/SKILL.md"));
        assert_eq!(name, "code-review");
        assert_eq!(desc, "Review a diff");
        assert_eq!(body.trim(), "Body text here.");
    }
}
