//! The project registry.
//!
//! `wingman board` is the only multi-project surface in the CLI — every other
//! pilot command resolves exactly one project root. The registry is populated
//! automatically: any `pilot` or `board` command calls [`BoardStore::touch_project`]
//! with the resolved root. No config to write, no repo to remember to add.
//!
//! Auto-registration without an eviction path is how a board fills with dead
//! repos, so `hidden` exists and `--forget` is **sticky**: continuing to work
//! in a forgotten repo does not silently bring it back. `--restore` does.

use std::path::{Path, PathBuf};

use crate::store::{now, BoardError, BoardStore, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub root: PathBuf,
    pub name: String,
    pub last_seen: String,
    pub hidden: bool,
}

impl Project {
    /// Whether the project root still exists on disk. A moved or deleted repo
    /// keeps its cards — they render with a `missing` badge instead of
    /// vanishing, and `--relocate` repairs the path.
    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }
}

impl BoardStore {
    /// Register `root`, or refresh its `last_seen`. Returns the project id.
    ///
    /// Idempotent and cheap enough to call at the top of every pilot command.
    /// Never clears `hidden` — see the module note on sticky forgetting.
    pub fn touch_project(&self, root: &Path) -> Result<String> {
        let root = canonical(root);
        let key = root.to_string_lossy().to_string();

        if let Some(id) = self.project_id_for_root(&root)? {
            self.lock().execute(
                "UPDATE project SET last_seen = ?1 WHERE id = ?2",
                (now(), &id),
            )?;
            return Ok(id);
        }

        let name = display_name(&root);
        let id = self.unique_slug(&slugify(&name))?;
        self.lock().execute(
            "INSERT INTO project (id, root, name, last_seen, hidden)
             VALUES (?1, ?2, ?3, ?4, 0)",
            (&id, &key, &name, now()),
        )?;
        Ok(id)
    }

    pub fn project_id_for_root(&self, root: &Path) -> Result<Option<String>> {
        let key = canonical(root).to_string_lossy().to_string();
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id FROM project WHERE root = ?1")?;
        let mut rows = stmt.query([&key])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    /// All registered projects, hidden ones only when `include_hidden`.
    pub fn projects(&self, include_hidden: bool) -> Result<Vec<Project>> {
        let conn = self.lock();
        let sql = if include_hidden {
            "SELECT id, root, name, last_seen, hidden FROM project ORDER BY name"
        } else {
            "SELECT id, root, name, last_seen, hidden FROM project WHERE hidden = 0 ORDER BY name"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                root: PathBuf::from(r.get::<_, String>(1)?),
                name: r.get(2)?,
                last_seen: r.get(3)?,
                hidden: r.get::<_, i64>(4)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn project(&self, id: &str) -> Result<Project> {
        self.projects(true)?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| BoardError::NoSuchProject(id.to_string()))
    }

    /// Hide a project. Cards are preserved; `restore_project` brings it back.
    pub fn forget_project(&self, id: &str) -> Result<()> {
        self.set_hidden(id, true)
    }

    pub fn restore_project(&self, id: &str) -> Result<()> {
        self.set_hidden(id, false)
    }

    fn set_hidden(&self, id: &str, hidden: bool) -> Result<()> {
        let n = self.lock().execute(
            "UPDATE project SET hidden = ?1 WHERE id = ?2",
            (i64::from(hidden), id),
        )?;
        if n == 0 {
            return Err(BoardError::NoSuchProject(id.to_string()));
        }
        Ok(())
    }

    /// Point a registered project at a new path, for a repo that moved.
    pub fn relocate_project(&self, id: &str, root: &Path) -> Result<()> {
        let root = canonical(root);
        if !root.is_dir() {
            return Err(BoardError::Invalid(format!(
                "{} is not a directory",
                root.display()
            )));
        }
        let n = self.lock().execute(
            "UPDATE project SET root = ?1, last_seen = ?2 WHERE id = ?3",
            (root.to_string_lossy().to_string(), now(), id),
        )?;
        if n == 0 {
            return Err(BoardError::NoSuchProject(id.to_string()));
        }
        Ok(())
    }

    /// Import `[[serve.projects]]` once, so a `serve` user opens a populated
    /// board. Guarded by a `meta` key rather than by emptiness — a user who
    /// forgets every imported project should not have them all come back.
    pub fn import_serve_projects(&self, roots: &[PathBuf]) -> Result<usize> {
        if self
            .get_meta("serve_import_done")
            .is_ok_and(|v| v.is_some())
        {
            return Ok(0);
        }
        let mut n = 0;
        for root in roots {
            if root.is_dir() {
                self.touch_project(root)?;
                n += 1;
            }
        }
        self.set_meta("serve_import_done", &now())?;
        Ok(n)
    }

    /// First free slug in the `name`, `name-2`, `name-3` … sequence.
    fn unique_slug(&self, base: &str) -> Result<String> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT 1 FROM project WHERE id = ?1")?;
        for n in 1..1000 {
            let candidate = if n == 1 {
                base.to_string()
            } else {
                format!("{base}-{n}")
            };
            if !stmt.exists([&candidate])? {
                return Ok(candidate);
            }
        }
        Err(BoardError::Invalid(format!(
            "cannot derive a unique project id from `{base}`"
        )))
    }
}

/// Best-effort canonicalisation. A path that does not exist yet (a repo on a
/// disconnected drive) is kept verbatim rather than erroring — the row is
/// still valid, and `Project::exists` reports the truth at render time.
///
/// Windows' `canonicalize` returns a `\\?\` extended-length path. It is
/// correct but it leaks into every path we print, so it is stripped here —
/// once, at the point paths enter the store — rather than at each display.
fn canonical(root: &Path) -> PathBuf {
    let c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        // UNC paths canonicalise to `\\?\UNC\server\share`; rewriting that
        // back to `\\server\share` keeps it a valid path.
        Some(rest) => PathBuf::from(match rest.strip_prefix("UNC\\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        }),
        None => c,
    }
}

/// Display name: the directory name.
fn display_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
}

/// Url-safe slug, matching `ServeProject::effective_id`'s spirit: lowercase,
/// non-alphanumerics collapsed to a single `-`, trimmed.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true; // leading dashes are trimmed
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::store;

    #[test]
    fn slugify_cases() {
        assert_eq!(slugify("Wingman"), "wingman");
        assert_eq!(slugify("My Repo!!"), "my-repo");
        assert_eq!(slugify("__--__"), "project");
        assert_eq!(slugify("a.b.c"), "a-b-c");
    }

    #[test]
    fn touch_is_stable_and_refreshes() {
        let (dir, s) = store();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        let a = s.touch_project(&root).unwrap();
        let b = s.touch_project(&root).unwrap();
        assert_eq!(a, b, "same root must map to the same id");
        assert_eq!(s.projects(false).unwrap().len(), 1);
        assert_eq!(a, "repo");
    }

    #[test]
    fn slug_collision_gets_a_suffix() {
        let (dir, s) = store();
        let a = dir.path().join("x").join("repo");
        let b = dir.path().join("y").join("repo");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        assert_eq!(s.touch_project(&a).unwrap(), "repo");
        assert_eq!(s.touch_project(&b).unwrap(), "repo-2");
    }

    #[test]
    fn forget_is_sticky_across_touches() {
        let (dir, s) = store();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let id = s.touch_project(&root).unwrap();

        s.forget_project(&id).unwrap();
        assert!(s.projects(false).unwrap().is_empty());

        // Working in the repo again must NOT un-forget it.
        s.touch_project(&root).unwrap();
        assert!(s.projects(false).unwrap().is_empty());
        assert_eq!(s.projects(true).unwrap().len(), 1);

        s.restore_project(&id).unwrap();
        assert_eq!(s.projects(false).unwrap().len(), 1);
    }

    #[test]
    fn relocate_moves_the_root() {
        let (dir, s) = store();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let id = s.touch_project(&old).unwrap();

        s.relocate_project(&id, &new).unwrap();
        assert_eq!(s.project(&id).unwrap().root, canonical(&new));
        assert!(s.relocate_project(&id, &dir.path().join("nope")).is_err());
    }

    #[test]
    fn serve_import_runs_once() {
        let (dir, s) = store();
        let root = dir.path().join("served");
        std::fs::create_dir_all(&root).unwrap();

        assert_eq!(
            s.import_serve_projects(std::slice::from_ref(&root))
                .unwrap(),
            1
        );
        assert_eq!(s.import_serve_projects(&[root]).unwrap(), 0);
    }

    #[test]
    fn missing_project_errors() {
        let (_d, s) = store();
        assert!(s.project("nope").is_err());
        assert!(s.forget_project("nope").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn canonical_strips_the_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let got = canonical(dir.path());
        assert!(
            !got.to_string_lossy().starts_with(r"\?\"),
            "verbatim prefix leaked: {}",
            got.display()
        );
        assert!(got.is_dir(), "stripped path must still resolve");
    }
}
