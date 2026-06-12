//! `arccode skill …` — skill utilities: extract drafts from session
//! history, and install shared skills from a URL or local file.

use anyhow::{bail, Context, Result};
use arccode_config::ProjectPaths;
use arccode_learn::extract::{extract_from_dir, write_drafts, ExtractConfig};
use std::process::ExitCode;

/// Install a skill from an https URL (raw markdown) or a local `.md` file
/// into the global skill library (or the project's with `--project`).
/// Skills are plain markdown with frontmatter, so any gist/repo raw URL
/// works as a sharing mechanism.
pub async fn install(
    source: String,
    name: Option<String>,
    project: bool,
    force: bool,
) -> Result<ExitCode> {
    let body = if source.starts_with("https://") || source.starts_with("http://") {
        if source.starts_with("http://") {
            bail!("refusing plain-http skill source; use https");
        }
        let resp = reqwest::get(&source).await.context("fetch skill")?;
        if !resp.status().is_success() {
            bail!("fetch failed: HTTP {}", resp.status());
        }
        resp.text().await.context("read skill body")?
    } else {
        std::fs::read_to_string(&source).with_context(|| format!("read {source}"))?
    };

    // Sanity-check shape: frontmatter (--- fences) with a non-empty body.
    let looks_like_skill = body.trim_start().starts_with("---")
        && body.matches("---").count() >= 2
        && !body.trim().is_empty();
    if !looks_like_skill {
        bail!(
            "source doesn't look like a skill (expected markdown with `---` \
             frontmatter containing name/description)"
        );
    }

    // Skill name: explicit flag > frontmatter `name:` > source file stem.
    let inferred = body
        .lines()
        .find(|l| l.trim_start().starts_with("name:"))
        .map(|l| l.trim_start().trim_start_matches("name:").trim().to_string())
        .filter(|s| !s.is_empty());
    let name = name
        .or(inferred)
        .or_else(|| {
            std::path::Path::new(&source)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .context("could not determine skill name; pass --name")?;
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();

    let dest = if project {
        let cwd = std::env::current_dir()?;
        arccode_skills::new_project_path(&cwd, &safe)
    } else {
        arccode_skills::new_global_path(&safe)
    }
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if dest.exists() && !force {
        bail!(
            "skill already exists at {} (pass --force to overwrite)",
            dest.display()
        );
    }
    std::fs::write(&dest, &body).with_context(|| format!("write {}", dest.display()))?;
    println!(
        "installed skill '{safe}' → {} ({} scope)",
        dest.display(),
        if project { "project" } else { "global" }
    );
    Ok(ExitCode::SUCCESS)
}

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
            "arccode: no repeated tool-call patterns found (scanned {})",
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

    let proposed_dir = arccode_config::ensure_global_dir()?
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
        println!("review and move into ~/.arccode/skills/ to promote.");
    }
    Ok(ExitCode::SUCCESS)
}
