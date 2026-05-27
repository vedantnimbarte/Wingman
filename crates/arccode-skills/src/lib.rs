//! Markdown-with-YAML-frontmatter skills.
//!
//! A *skill* is a reusable prompt prefix. It lives in either:
//!   - `~/.arccode/skills/<name>.md`   (global)
//!   - `<project>/.arccode/skills/<name>.md`  (project — overrides global)
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

use arccode_config::ProjectPaths;

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

    if let Ok(global) = arccode_config::ensure_global_dir() {
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

/// Path under `~/.arccode/skills/` where `/skills new` should write a new
/// skill. Creates the directory if missing.
pub fn new_global_path(name: &str) -> Result<PathBuf, SkillError> {
    let dir = arccode_config::ensure_global_dir()
        .map_err(|e| SkillError::Io(format!("{e}")))?
        .join("skills");
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
    let name = fm
        .get("name")
        .cloned()
        .unwrap_or(stem)
        .trim()
        .to_string();
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
    if let Some(rest) = trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n")) {
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
}
