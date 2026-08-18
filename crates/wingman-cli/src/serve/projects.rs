//! Project allowlist: which repos this server will touch, and nothing else.
//!
//! A request names a project by id (or by its exact configured root). Anything
//! that does not resolve is a 404 — so a stolen token reaches the repos you
//! listed and no other directory on the machine.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use wingman_config::{ServeConfig, ServeProject};

/// A resolved entry from `[[serve.projects]]`.
#[derive(Debug, Clone)]
pub struct Project {
    pub id: String,
    /// Canonicalised root. Canonical form is what containment checks compare
    /// against, so `..` and symlink games cannot smuggle a path out.
    pub root: PathBuf,
}

/// Resolve and validate the whole allowlist.
pub fn resolve_all(cfg: &ServeConfig) -> Result<Vec<Project>> {
    if cfg.projects.is_empty() {
        return Err(anyhow!(
            "refusing to start: no [[serve.projects]] configured, so there is nothing to serve.\n\
             Add one:\n\n  [[serve.projects]]\n  id   = \"myrepo\"\n  root = \"/path/to/repo\"\n"
        ));
    }

    let mut out: Vec<Project> = Vec::with_capacity(cfg.projects.len());
    for entry in &cfg.projects {
        let project = resolve_one(entry)?;
        if let Some(dup) = out.iter().find(|p| p.id == project.id) {
            return Err(anyhow!(
                "refusing to start: two projects share the id '{}' ({} and {}). \
                 Give one an explicit `id`.",
                project.id,
                dup.root.display(),
                project.root.display()
            ));
        }
        out.push(project);
    }
    Ok(out)
}

fn resolve_one(entry: &ServeProject) -> Result<Project> {
    let root = entry.root.canonicalize().map_err(|e| {
        anyhow!(
            "refusing to start: project root {} is not readable: {e}",
            entry.root.display()
        )
    })?;
    if !root.is_dir() {
        return Err(anyhow!(
            "refusing to start: project root {} is not a directory",
            root.display()
        ));
    }
    let id = entry.effective_id();
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "project id '{id}' is not URL-safe (use letters, digits, '-' and '_')"
        ));
    }
    Ok(Project { id, root })
}

/// Look up a project by id, or by a path that resolves to a configured root.
pub fn find<'a>(projects: &'a [Project], key: &str) -> Option<&'a Project> {
    if let Some(p) = projects.iter().find(|p| p.id == key) {
        return Some(p);
    }
    // Accept the root path too, so a client that knows the directory does not
    // need the id. Canonicalised on both sides: `/repo/.` and `/repo` are the
    // same project, and a non-existent path matches nothing.
    let candidate = Path::new(key).canonicalize().ok()?;
    projects.iter().find(|p| p.root == candidate)
}

/// Is `candidate` inside `root`? Used to reject body-supplied paths that
/// point outside the project they claim to belong to.
///
/// Both sides are canonicalised, so this cannot be defeated by `..` segments
/// or a symlink pointing out of the tree. A path that does not exist yet is
/// checked via its nearest existing ancestor — a create-file request must
/// still land inside the project.
#[allow(dead_code)] // phase 3/4: guards body-supplied paths
pub fn contains(root: &Path, candidate: &Path) -> bool {
    let mut probe = candidate.to_path_buf();
    let resolved = loop {
        if let Ok(c) = probe.canonicalize() {
            break c;
        }
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent.to_path_buf(),
            _ => return false,
        }
    };
    resolved.starts_with(root)
}

/// JSON view of a project for `GET /v1/projects`.
pub fn describe(project: &Project) -> Value {
    let wingman_dir = project.root.join(".wingman");
    let index_db = wingman_dir.join("index.db");
    let index_age_secs = std::fs::metadata(&index_db)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    json!({
        "id": project.id,
        "root": project.root.to_string_lossy(),
        "branch": current_branch(&project.root),
        "indexd_running": wingman_dir.join("indexd.pid").exists(),
        "index_age_secs": index_age_secs,
    })
}

/// Current git branch, read from `.git/HEAD` rather than by shelling out to
/// git — this runs on every `GET /v1/projects` and a fork per project per
/// request is a silly price for one line of a file.
fn current_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => Some(branch.to_string()),
        // Detached HEAD: report the short commit.
        None => Some(head.chars().take(8).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(projects: Vec<ServeProject>) -> ServeConfig {
        ServeConfig {
            projects,
            ..Default::default()
        }
    }

    #[test]
    fn empty_allowlist_refuses() {
        assert!(resolve_all(&cfg_with(vec![])).is_err());
    }

    #[test]
    fn missing_root_refuses() {
        let cfg = cfg_with(vec![ServeProject {
            id: Some("gone".into()),
            root: PathBuf::from("/definitely/not/here/at/all"),
        }]);
        assert!(resolve_all(&cfg).is_err());
    }

    #[test]
    fn duplicate_ids_refuse() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let cfg = cfg_with(vec![
            ServeProject {
                id: Some("same".into()),
                root: a.path().to_path_buf(),
            },
            ServeProject {
                id: Some("same".into()),
                root: b.path().to_path_buf(),
            },
        ]);
        assert!(resolve_all(&cfg).is_err());
    }

    #[test]
    fn id_defaults_to_directory_name_and_lookup_works_by_id_or_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        std::fs::create_dir(&repo).unwrap();
        let cfg = cfg_with(vec![ServeProject {
            id: None,
            root: repo.clone(),
        }]);
        let projects = resolve_all(&cfg).unwrap();
        assert_eq!(projects[0].id, "myrepo");
        assert!(find(&projects, "myrepo").is_some());
        assert!(find(&projects, &repo.to_string_lossy()).is_some());
        assert!(find(&projects, "nope").is_none());
    }

    #[test]
    fn non_url_safe_id_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(vec![ServeProject {
            id: Some("bad id/../".into()),
            root: dir.path().to_path_buf(),
        }]);
        assert!(resolve_all(&cfg).is_err());
    }

    #[test]
    fn containment_rejects_escapes_and_accepts_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("inside.txt"), "x").unwrap();
        assert!(contains(&root, &root.join("inside.txt")));
        // A file that does not exist yet, but whose parent is in the tree.
        assert!(contains(&root, &root.join("new.txt")));
        // Escapes, both literal and via `..`.
        assert!(!contains(&root, Path::new("/etc/passwd")));
        assert!(!contains(&root, &root.join("..").join("outside.txt")));
    }
}
