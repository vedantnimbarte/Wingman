//! Markdown-with-YAML-frontmatter skills.
//!
//! A *skill* is a reusable prompt prefix. It lives in either:
//!   - `~/.wingman/skills/<name>.md`   (global)
//!   - `<project>/.wingman/skills/<name>.md`  (project — overrides global)
//!
//! File format:
//!
//! ```markdown
//! ---
//! name: code-review
//! description: Review the staged diff for correctness and style
//! ---
//! You are a careful code reviewer. Look at the staged changes and...
//! ```
//!
//! The frontmatter parser is intentionally tiny — we recognize `name:` and
//! `description:` lines and ignore anything else. No YAML dep required.

use std::path::{Path, PathBuf};

use wingman_config::ProjectPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Global,
    Project,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Skill {
    /// Stable id used for lookup. Defaults to the file stem if not in
    /// frontmatter.
    pub name: String,
    pub description: String,
    /// The instruction body — everything after the frontmatter.
    pub body: String,
    /// Where this skill came from, for display in `/skills`.
    pub source: SkillSource,
    /// File this skill was loaded from. Used by `/skills new`'s editor
    /// launch and for "open in editor" affordances later.
    pub path: PathBuf,
}

/// Walk both skill directories and merge results. Project skills override
/// global skills of the same name.
pub fn load_all(project_root: &Path) -> Vec<Skill> {
    let mut by_name: std::collections::BTreeMap<String, Skill> = Default::default();

    if let Ok(global) = wingman_config::ensure_global_dir() {
        for s in load_dir(&global.join("skills"), SkillSource::Global) {
            by_name.insert(s.name.clone(), s);
        }
    }

    let project_dir = ProjectPaths::discover(project_root).dir.join("skills");
    for s in load_dir(&project_dir, SkillSource::Project) {
        by_name.insert(s.name.clone(), s);
    }

    by_name.into_values().collect()
}

/// Path under `~/.wingman/skills/` where `/skills new` should write a new
/// skill. Creates the directory if missing.
pub fn new_global_path(name: &str) -> Result<PathBuf, SkillError> {
    let dir = wingman_config::ensure_global_dir()
        .map_err(|e| SkillError::Io(format!("{e}")))?
        .join("skills");
    std::fs::create_dir_all(&dir).map_err(|e| SkillError::Io(format!("{e}")))?;
    Ok(dir.join(format!("{name}.md")))
}

/// Path under `<project>/.wingman/skills/` where a project-scoped skill
/// should be written. Creates the directory if missing.
pub fn new_project_path(project_root: &Path, name: &str) -> Result<PathBuf, SkillError> {
    let dir = ProjectPaths::discover(project_root).dir.join("skills");
    std::fs::create_dir_all(&dir).map_err(|e| SkillError::Io(format!("{e}")))?;
    Ok(dir.join(format!("{name}.md")))
}

/// Starter content for a brand-new skill file.
pub fn starter_template(name: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: <one-line summary of what this skill does>\n---\nYou are a helpful assistant. \
         Describe the persona / approach / constraints the model should adopt \
         when this skill is invoked.\n"
    )
}

/// Where a `/skills install`ed file should land.
#[derive(Debug, Clone, Copy)]
pub enum InstallScope {
    Global,
    Project,
}

/// A `/skills install <source>` argument, classified into something we can
/// fetch. Everything ultimately resolves to a URL that returns markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSourceKind {
    /// A direct `http(s)://` URL to a markdown file.
    Url(String),
    /// A bare name to resolve against the configured registry index.
    RegistryName(String),
}

/// Classify an install source:
/// - `http(s)://…`          → [`SkillSourceKind::Url`] verbatim
/// - `owner/repo[/path.md]` → a `raw.githubusercontent.com` URL
/// - anything else          → [`SkillSourceKind::RegistryName`]
pub fn classify_source(source: &str) -> SkillSourceKind {
    let s = source.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        return SkillSourceKind::Url(s.to_string());
    }
    if let Some(url) = github_raw_url(s) {
        return SkillSourceKind::Url(url);
    }
    SkillSourceKind::RegistryName(s.to_string())
}

/// Turn `owner/repo` or `owner/repo/path` into a raw GitHub URL on the
/// default branch (`HEAD`). Returns `None` if `spec` doesn't look like a
/// repo path. A bare `owner/repo` fetches `SKILL.md`; a trailing path that
/// isn't already a `.md` file gets `/SKILL.md` appended.
fn github_raw_url(spec: &str) -> Option<String> {
    let parts: Vec<&str> = spec.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let safe = |p: &&str| {
        p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !parts.iter().all(safe) {
        return None;
    }
    let (owner, repo) = (parts[0], parts[1]);
    let path = match &parts[2..] {
        [] => "SKILL.md".to_string(),
        rest => {
            let joined = rest.join("/");
            if joined.ends_with(".md") {
                joined
            } else {
                format!("{joined}/SKILL.md")
            }
        }
    };
    Some(format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/HEAD/{path}"
    ))
}

/// Reduce an arbitrary skill name to a safe file stem — lowercase, only
/// `[a-z0-9._-]`, no leading/trailing `-`/`.`. Guards against path traversal
/// from an attacker-controlled `name:` field. `None` if nothing usable left.
fn sanitize_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '-' || c == '.').to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Validate downloaded markdown as a skill and write it into the chosen
/// skills dir, filed under its own (sanitised) `name`. `name_hint` is used
/// only when the frontmatter omits `name` (e.g. the URL basename).
/// Overwrites an existing same-named skill (install doubles as update).
pub fn install_markdown(
    content: &str,
    name_hint: &str,
    project_root: &Path,
    scope: InstallScope,
) -> Result<Skill, SkillError> {
    let (front, body) = split_frontmatter(content);
    if body.trim().is_empty() {
        return Err(SkillError::Io("downloaded skill body is empty".into()));
    }
    let fm = parse_frontmatter(front);
    let raw_name = fm
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| name_hint.to_string());
    let name = sanitize_name(&raw_name)
        .ok_or_else(|| SkillError::Io("skill has no usable name".into()))?;
    let (path, source) = match scope {
        InstallScope::Global => (new_global_path(&name)?, SkillSource::Global),
        InstallScope::Project => (new_project_path(project_root, &name)?, SkillSource::Project),
    };
    std::fs::write(&path, content).map_err(|e| SkillError::Io(format!("{e}")))?;
    load_one(&path, source)
}

fn load_dir(dir: &Path, source: SkillSource) -> Vec<Skill> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match load_one(&path, source) {
            Ok(s) => out.push(s),
            Err(e) => tracing::warn!("skipping skill {}: {e}", path.display()),
        }
    }
    out
}

fn load_one(path: &Path, source: SkillSource) -> Result<Skill, SkillError> {
    let text = std::fs::read_to_string(path).map_err(|e| SkillError::Io(format!("{e}")))?;
    let (front, body) = split_frontmatter(&text);
    let fm = parse_frontmatter(front);

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();
    let name = fm.get("name").cloned().unwrap_or(stem).trim().to_string();
    let description = fm
        .get("description")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(Skill {
        name,
        description,
        body: body.trim().to_string(),
        source,
        path: path.to_path_buf(),
    })
}

/// Split out a leading `---`-delimited frontmatter block. Returns
/// `(frontmatter, body)`. If the file doesn't start with `---`, the entire
/// content is treated as body.
fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches('\u{FEFF}'); // BOM guard
    if let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        if let Some(end) = find_closing_fence(rest) {
            let (front, after) = rest.split_at(end.0);
            // Skip the closing fence and its newline.
            let body = &after[end.1..];
            return (front, body);
        }
    }
    ("", text)
}

/// Returns (start_index_of_fence, fence_length_with_newline).
fn find_closing_fence(s: &str) -> Option<(usize, usize)> {
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\n', '\r']);
        if stripped == "---" {
            return Some((pos, line.len()));
        }
        pos += line.len();
    }
    None
}

fn parse_frontmatter(front: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            out.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    out
}

/// Extract all `{{var}}` placeholder names from a skill body, in order of
/// first appearance, deduplicated.
pub fn extract_vars(body: &str) -> Vec<String> {
    let mut seen = std::collections::LinkedList::new();
    let mut set = std::collections::HashSet::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'{') {
            chars.next(); // consume second '{'
            let mut name = String::new();
            for c2 in chars.by_ref() {
                if c2 == '}' {
                    // consume potential second '}'
                    break;
                }
                name.push(c2);
            }
            // consume closing '}'
            chars.next();
            let name = name.trim().to_string();
            if !name.is_empty() && set.insert(name.clone()) {
                seen.push_back(name);
            }
        }
    }
    seen.into_iter().collect()
}

/// Replace all `{{var}}` placeholders in `body` with the values from `vars`.
/// Unknown variable names are left as-is.
pub fn apply_vars(body: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut result = body.to_string();
    for (k, v) in vars {
        result = result.replace(&format!("{{{{{k}}}}}"), v);
    }
    result
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("io: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter() {
        let text = "---\nname: x\ndescription: y\n---\nbody here";
        let (front, body) = split_frontmatter(text);
        assert_eq!(front, "name: x\ndescription: y\n");
        assert_eq!(body, "body here");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let text = "just some content";
        let (front, body) = split_frontmatter(text);
        assert_eq!(front, "");
        assert_eq!(body, text);
    }

    #[test]
    fn parses_basic_kv() {
        let m = parse_frontmatter("name: foo\ndescription: bar baz\n");
        assert_eq!(m.get("name").map(|s| s.as_str()), Some("foo"));
        assert_eq!(m.get("description").map(|s| s.as_str()), Some("bar baz"));
    }

    #[test]
    fn classifies_install_sources() {
        use SkillSourceKind::*;
        assert_eq!(
            classify_source("https://x.com/a.md"),
            Url("https://x.com/a.md".into())
        );
        // owner/repo → raw GitHub SKILL.md
        assert_eq!(
            classify_source("acme/skills"),
            Url("https://raw.githubusercontent.com/acme/skills/HEAD/SKILL.md".into())
        );
        // owner/repo/path.md kept verbatim
        assert_eq!(
            classify_source("acme/skills/pack/review.md"),
            Url("https://raw.githubusercontent.com/acme/skills/HEAD/pack/review.md".into())
        );
        // bare name → registry lookup
        assert_eq!(classify_source("code-review"), RegistryName("code-review".into()));
    }

    #[test]
    fn sanitize_blocks_path_traversal() {
        assert_eq!(sanitize_name("../../etc/passwd").as_deref(), Some("etc-passwd"));
        assert_eq!(sanitize_name("Good Skill!").as_deref(), Some("good-skill"));
        assert_eq!(sanitize_name("///"), None);
    }

    #[test]
    fn install_project_roundtrip() {
        let root = std::env::temp_dir().join(format!("wingman-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let md = "---\nname: Demo Skill\ndescription: does things\n---\nBe helpful and terse.\n";
        let skill = install_markdown(md, "fallback", &root, InstallScope::Project).unwrap();
        // Lookup name comes from frontmatter; the on-disk filename is sanitised.
        assert_eq!(skill.name, "Demo Skill");
        assert_eq!(skill.path.file_name().unwrap(), "demo-skill.md");
        // the normal loader now finds it with no extra registration step
        let all = load_all(&root);
        assert!(all
            .iter()
            .any(|s| s.name == "Demo Skill" && s.description == "does things"));
        std::fs::remove_dir_all(&root).ok();
    }
}
