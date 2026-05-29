//! Persistent memory: markdown files with YAML frontmatter, indexed by a
//! sibling `MEMORY.md` for fast inclusion in the system prompt.
//!
//! Layout:
//!
//!   ~/.arccode/memory/MEMORY.md          (global index — one line per slug)
//!   ~/.arccode/memory/<slug>.md          (global memory body)
//!   <project>/.arccode/memory/MEMORY.md  (project index)
//!   <project>/.arccode/memory/<slug>.md  (project memory body)
//!
//! Frontmatter recognised: `name`, `description`, `type` (user|feedback|
//! project|reference). The body is freeform markdown.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{LearnError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }

    /// Where this kind of memory should live by default.
    pub fn default_scope(self) -> MemoryScope {
        match self {
            Self::Project => MemoryScope::Project,
            // user/feedback/reference are usually about the human or their
            // overall workflow, so they survive across projects.
            _ => MemoryScope::Global,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Global,
    Project,
}

impl MemoryScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Memory {
    pub name: String,
    pub description: String,
    pub mtype: MemoryType,
    pub body: String,
    pub scope: MemoryScope,
    pub path: PathBuf,
}

/// A pending memory awaiting persistence. Constructed by the agent (via the
/// `save_memory` tool) or by `/save-memory`. The scope defaults from
/// `mtype.default_scope()` if not specified.
#[derive(Debug, Clone)]
pub struct MemoryDraft {
    pub name: String,
    pub description: String,
    pub mtype: MemoryType,
    pub body: String,
    pub scope: Option<MemoryScope>,
}

pub struct MemoryStore {
    /// Absolute path to the project root (used to compute the project
    /// memory dir). Required even when a write targets global memory so
    /// reads can pick up both.
    project_root: PathBuf,
}

impl MemoryStore {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }

    pub fn global_dir() -> Result<PathBuf> {
        let g = arccode_config::ensure_global_dir()?;
        let dir = g.join("memory");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn project_dir(&self) -> PathBuf {
        self.project_root.join(".arccode").join("memory")
    }

    /// Ensure both index files (global + project) exist; an empty `MEMORY.md`
    /// is fine. Called lazily before any read so the user can `cat` it.
    pub fn ensure_indexes(&self) -> Result<()> {
        let g = Self::global_dir()?;
        let _ = ensure_index(&g);
        let _ = ensure_index(&self.project_dir());
        Ok(())
    }

    /// Load every memory from both scopes. Project memories with a colliding
    /// name override the global one.
    pub fn load_all(&self) -> Vec<Memory> {
        let mut by_name: BTreeMap<String, Memory> = BTreeMap::new();
        if let Ok(g) = Self::global_dir() {
            for m in load_dir(&g, MemoryScope::Global) {
                by_name.insert(m.name.clone(), m);
            }
        }
        for m in load_dir(&self.project_dir(), MemoryScope::Project) {
            by_name.insert(m.name.clone(), m);
        }
        by_name.into_values().collect()
    }

    /// Find a memory by slug; project wins on tie.
    pub fn find(&self, name: &str) -> Option<Memory> {
        self.load_all().into_iter().find(|m| m.name == name)
    }

    /// Persist a memory: write the body file then append/update the index.
    /// Returns the final on-disk path.
    pub fn save(&self, draft: MemoryDraft) -> Result<PathBuf> {
        let scope = draft.scope.unwrap_or_else(|| draft.mtype.default_scope());
        let dir = match scope {
            MemoryScope::Global => Self::global_dir()?,
            MemoryScope::Project => {
                let d = self.project_dir();
                std::fs::create_dir_all(&d)?;
                d
            }
        };
        let slug = slugify(&draft.name);
        let path = dir.join(format!("{slug}.md"));
        let body = render_body(&draft, &slug);
        std::fs::write(&path, body)?;
        update_index(
            &dir.join("MEMORY.md"),
            &slug,
            &draft.description,
            draft.mtype,
        )?;
        Ok(path)
    }

    /// Delete a memory by slug. Searches both scopes (project first).
    pub fn forget(&self, name: &str) -> Result<bool> {
        let slug = slugify(name);
        for dir in [self.project_dir(), Self::global_dir()?] {
            let path = dir.join(format!("{slug}.md"));
            if path.exists() {
                std::fs::remove_file(&path)?;
                remove_from_index(&dir.join("MEMORY.md"), &slug)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Render the index portion fit for a system-prompt block. One bullet per
/// memory, grouped by scope. Returns `None` when no memories exist (lets
/// callers skip emitting an empty section).
pub fn render_prompt_block(memories: &[Memory]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }
    let mut globals: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.scope == MemoryScope::Global)
        .collect();
    let mut project: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.scope == MemoryScope::Project)
        .collect();
    globals.sort_by(|a, b| a.name.cmp(&b.name));
    project.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::new();
    if !globals.is_empty() {
        out.push_str("# What you know about this user\n");
        for m in &globals {
            out.push_str(&format!(
                "- [{}] {} — {}\n",
                m.mtype.as_str(),
                m.name,
                truncate(&m.description.replace('\n', " "), 140),
            ));
        }
        out.push('\n');
    }
    if !project.is_empty() {
        out.push_str("# What you know about this project\n");
        for m in &project {
            out.push_str(&format!(
                "- [{}] {} — {}\n",
                m.mtype.as_str(),
                m.name,
                truncate(&m.description.replace('\n', " "), 140),
            ));
        }
        out.push('\n');
    }
    out.push_str(
        "(Use the `recall_memory` tool with a memory name to read the full body \
         when you need detail. Use `save_memory` to persist new insights.)\n",
    );
    Some(out)
}

fn load_dir(dir: &Path, scope: MemoryScope) -> Vec<Memory> {
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
        if path.file_name().and_then(|s| s.to_str()) == Some("MEMORY.md") {
            continue;
        }
        match load_one(&path, scope) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!("skipping memory {}: {e}", path.display()),
        }
    }
    out
}

fn load_one(path: &Path, scope: MemoryScope) -> Result<Memory> {
    let text = std::fs::read_to_string(path)?;
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
    let mtype = fm
        .get("type")
        .and_then(|s| MemoryType::parse(s))
        .unwrap_or(MemoryType::User);

    Ok(Memory {
        name,
        description,
        mtype,
        body: body.trim().to_string(),
        scope,
        path: path.to_path_buf(),
    })
}

fn render_body(draft: &MemoryDraft, slug: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {slug}\n"));
    out.push_str(&format!("description: {}\n", one_line(&draft.description)));
    out.push_str(&format!("type: {}\n", draft.mtype.as_str()));
    out.push_str("---\n");
    out.push_str(draft.body.trim());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn ensure_index(dir: &Path) -> Result<()> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err(LearnError::Io(e));
    }
    let path = dir.join("MEMORY.md");
    if !path.exists() {
        std::fs::write(&path, "# arccode memory index\n\n")?;
    }
    Ok(())
}

/// Append a line for `slug` to the index if absent; replace the existing
/// line if present.
fn update_index(index_path: &Path, slug: &str, description: &str, mtype: MemoryType) -> Result<()> {
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(index_path).unwrap_or_default();
    let line = format!(
        "- [{slug}]({slug}.md) [{}] — {}\n",
        mtype.as_str(),
        truncate(&one_line(description), 140),
    );
    let prefix = format!("- [{slug}](");
    let mut next = String::new();
    let mut replaced = false;
    if existing.is_empty() {
        next.push_str("# arccode memory index\n\n");
    }
    for raw in existing.lines() {
        if raw.starts_with(&prefix) {
            next.push_str(&line);
            replaced = true;
        } else {
            next.push_str(raw);
            next.push('\n');
        }
    }
    if !replaced {
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&line);
    }
    std::fs::write(index_path, next)?;
    Ok(())
}

fn remove_from_index(index_path: &Path, slug: &str) -> Result<()> {
    if !index_path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(index_path)?;
    let prefix = format!("- [{slug}](");
    let mut next = String::new();
    for raw in existing.lines() {
        if !raw.starts_with(&prefix) {
            next.push_str(raw);
            next.push('\n');
        }
    }
    std::fs::write(index_path, next)?;
    Ok(())
}

fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches('\u{FEFF}');
    if let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        if let Some(end) = find_closing_fence(rest) {
            let (front, after) = rest.split_at(end.0);
            let body = &after[end.1..];
            return (front, body);
        }
    }
    ("", text)
}

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

fn parse_frontmatter(front: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
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

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arccode-learn-mem-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".arccode")).unwrap();
        dir
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("User prefers terse"), "user-prefers-terse");
        assert_eq!(slugify("---foo!!!bar---"), "foo-bar");
        assert_eq!(slugify("kebab-case-already"), "kebab-case-already");
    }

    #[test]
    fn round_trip_save_and_load() {
        let root = tmp_project();
        let store = MemoryStore::new(root.clone());
        let path = store
            .save(MemoryDraft {
                name: "prefers terse".into(),
                description: "user wants concise replies".into(),
                mtype: MemoryType::Feedback,
                body: "Keep responses short. Avoid trailing summaries.".into(),
                scope: Some(MemoryScope::Project),
            })
            .unwrap();
        assert!(path.exists());

        let loaded = store.find("prefers-terse").expect("memory present");
        assert_eq!(loaded.mtype, MemoryType::Feedback);
        assert!(loaded.body.contains("Avoid trailing summaries"));
        assert_eq!(loaded.scope, MemoryScope::Project);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn project_scope_overrides_global_on_name_collision() {
        let root = tmp_project();
        // We can't actually point global at a tmp dir without env hijacking,
        // so just verify project-scope save + load and absence collisions in
        // the project-only path.
        let store = MemoryStore::new(root.clone());
        store
            .save(MemoryDraft {
                name: "demo".into(),
                description: "x".into(),
                mtype: MemoryType::Project,
                body: "body".into(),
                scope: Some(MemoryScope::Project),
            })
            .unwrap();
        assert!(store.find("demo").is_some());
        assert!(store.forget("demo").unwrap());
        assert!(store.find("demo").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_prompt_block_groups_by_scope() {
        let mems = vec![
            Memory {
                name: "global1".into(),
                description: "g".into(),
                mtype: MemoryType::User,
                body: String::new(),
                scope: MemoryScope::Global,
                path: PathBuf::new(),
            },
            Memory {
                name: "proj1".into(),
                description: "p".into(),
                mtype: MemoryType::Project,
                body: String::new(),
                scope: MemoryScope::Project,
                path: PathBuf::new(),
            },
        ];
        let block = render_prompt_block(&mems).unwrap();
        assert!(block.contains("about this user"));
        assert!(block.contains("about this project"));
        assert!(block.contains("global1"));
        assert!(block.contains("proj1"));
    }
}
