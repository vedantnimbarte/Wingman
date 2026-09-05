//! Filesystem layout helpers.
//!
//! Global:  `~/.wingman/`            (config, credentials, model cache, logs)
//! Project: `<project>/.wingman/`    (sessions, repo index, project overrides)
//!
//! Project root discovery walks up from the start dir looking for the first
//! ancestor (other than the user's home directory) that contains a `.git`
//! directory or a `.wingman` directory. If neither marker is found, the
//! start dir itself is treated as the project root. The home directory is
//! excluded because the global `~/.wingman/` would otherwise be mistaken
//! for a project marker on any unparented working dir.

use crate::ConfigError;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Relocates the global directory. Named and shaped after `CARGO_HOME` and
/// `RUSTUP_HOME`: it **is** the directory, not the home to hang `.wingman` off.
pub const HOME_ENV: &str = "WINGMAN_HOME";

/// The user's actual home directory, whatever [`HOME_ENV`] says.
///
/// This is for other tools' dotfiles — `~/.claude/skills`, a skill pack's
/// install root — which live beside the global dir today but are not part of
/// it. Callers used to reach them with `global_dir().parent()`, which worked
/// only because the global dir was always `~/.wingman`; with the override that
/// coincidence would point them at a scratch directory's parent.
///
/// Use [`global_dir`] for anything that belongs to Wingman.
pub fn user_home() -> Result<PathBuf, ConfigError> {
    Ok(directories::BaseDirs::new()
        .ok_or(ConfigError::NoHome)?
        .home_dir()
        .to_path_buf())
}

/// Resolve [`HOME_ENV`] once, keeping the outcome — error included.
///
/// Read once rather than per call for a reason beyond the syscall: a process
/// whose global directory moved underneath it would write credentials to one
/// place and read them from another. `std::env::set_var` is also unsound to
/// race in a threaded program, and this crate is used from one.
///
/// Tests do not set this. Everything that needs a temporary global dir takes
/// one as a parameter — `inbox::append_to`, `read_open`, `ReplyReader::at_end_in`
/// — which works under parallel tests, as an env var never could.
/// `Err` carries `(value, reason)` rather than a `ConfigError`, which is not
/// `Clone` — it holds the toml crate's errors, and making it clonable to cache
/// one bad env var would be the tail wagging the dog.
fn resolved_override() -> &'static Result<Option<PathBuf>, (String, String)> {
    static RESOLVED: OnceLock<Result<Option<PathBuf>, (String, String)>> = OnceLock::new();
    RESOLVED.get_or_init(|| validate_home(std::env::var_os(HOME_ENV).as_deref()))
}

/// The rules, separated from the environment so both outcomes are testable.
///
/// An env var cannot be varied inside a parallel test binary — `set_var` is
/// unsound to race and [`resolved_override`] caches — so the logic has to be
/// reachable without one.
fn validate_home(raw: Option<&std::ffi::OsStr>) -> Result<Option<PathBuf>, (String, String)> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    if path.to_string_lossy().trim().is_empty() {
        // An empty value is how a shell spells "unset" by accident
        // (`WINGMAN_HOME=$UNSET wingman …`), and rooting the whole global
        // directory at "" would be a spectacular way to honour it.
        return Ok(None);
    }
    if path.is_relative() {
        // Resolving this against the current directory would bind the global
        // dir to whichever cwd happened to be current at first use — and
        // project resolution here already moves the process cwd. A refusal at
        // startup beats a config directory that moves underneath the process.
        return Err((
            path.display().to_string(),
            "must be an absolute path".to_string(),
        ));
    }
    Ok(Some(path))
}

/// Returns `~/.wingman/`, or [`HOME_ENV`] when it is set. Pure path
/// computation — does **not** create.
///
/// The override exists so a whole Wingman install can be pointed at a scratch
/// directory: running `serve` against throwaway config and credentials, a
/// sandboxed agent, or two versions side by side. It deliberately does not move
/// `~/.claude/settings.json`, which the Claude Code hook import reads — that is
/// another tool's directory, and relocating it from *our* env var would be
/// presumptuous.
pub fn global_dir() -> Result<PathBuf, ConfigError> {
    match resolved_override() {
        Ok(Some(dir)) => Ok(dir.clone()),
        Ok(None) => Ok(user_home()?.join(".wingman")),
        Err((value, reason)) => Err(ConfigError::BadEnv {
            name: HOME_ENV.to_string(),
            value: value.clone(),
            reason: reason.clone(),
        }),
    }
}

/// Returns `~/.wingman/`, creating it on demand.
pub fn ensure_global_dir() -> Result<PathBuf, ConfigError> {
    let dir = global_dir()?;
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })?;
    }
    Ok(dir)
}

/// Returns `<project>/.wingman/`. Pure path computation.
pub fn project_dir(project_root: &Path) -> PathBuf {
    project_root.join(".wingman")
}

/// Walks up from `start` looking for `.git` or `.wingman`. The user's home
/// directory is never returned as a project root (the global `~/.wingman/`
/// would otherwise be a false positive). Falls back to `start` itself if
/// no marker is found.
/// The project that *owns* `start`, looking through a pilot worktree.
///
/// [`find_project_root`] deliberately stops at a pilot worktree: a worker's
/// file access should be contained to its own branch, so that is the right
/// root for tool containment. It is the wrong root for anything that must
/// outlive the task, because worktrees live at
/// `<project>/.wingman/worktrees/<name>` and are force-removed at cleanup —
/// a worker's session log written under one is deleted with it.
///
/// Walks out to the owning project when `start` is inside a worktree, and is
/// otherwise identical to [`find_project_root`].
pub fn find_owning_project_root(start: &Path) -> PathBuf {
    let here = find_project_root(start);
    // `<project>/.wingman/worktrees/<name>` — recognised by the two
    // components above it, not by the directory's own name, so an ordinary
    // project that happens to be called "worktrees" is unaffected.
    let mut cursor = here.as_path();
    while let Some(parent) = cursor.parent() {
        if parent.file_name().is_some_and(|n| n == "worktrees")
            && parent
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == ".wingman")
        {
            if let Some(project) = parent.parent().and_then(|p| p.parent()) {
                return project.to_path_buf();
            }
        }
        cursor = parent;
    }
    here
}

pub fn find_project_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    if current.is_file() {
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        }
    }
    let home_dir = user_home().ok();
    let mut cursor: &Path = &current;
    loop {
        let is_home = home_dir.as_deref() == Some(cursor);
        if !is_home && (cursor.join(".git").exists() || cursor.join(".wingman").exists()) {
            return cursor.to_path_buf();
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => return current,
        }
    }
}

/// Bundle of paths for a given project.
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub config_file: PathBuf,
    pub sessions_dir: PathBuf,
    pub index_db: PathBuf,
}

impl ProjectPaths {
    pub fn from_root(root: PathBuf) -> Self {
        let dir = root.join(".wingman");
        Self {
            config_file: dir.join("config.toml"),
            sessions_dir: dir.join("sessions"),
            index_db: dir.join("index.db"),
            dir,
            root,
        }
    }

    pub fn discover(start: &Path) -> Self {
        Self::from_root(find_project_root(start))
    }
}

/// Path of the global `config.toml`.
pub fn global_config_path() -> Result<PathBuf, ConfigError> {
    Ok(global_dir()?.join("config.toml"))
}

/// Path of the global `credentials.toml`.
pub fn global_credentials_path() -> Result<PathBuf, ConfigError> {
    Ok(global_dir()?.join("credentials.toml"))
}

/// `~/.wingman/logs/`. Pure path computation.
pub fn global_logs_dir() -> Result<PathBuf, ConfigError> {
    Ok(global_dir()?.join("logs"))
}

/// `~/.wingman/logs/`, creating it on demand.
pub fn ensure_global_logs_dir() -> Result<PathBuf, ConfigError> {
    let dir = global_logs_dir()?;
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_paths_from_root_layout() {
        let pp = ProjectPaths::from_root(PathBuf::from("/tmp/proj"));
        assert!(pp.dir.ends_with(".wingman"));
        assert!(pp.config_file.ends_with("config.toml"));
        assert!(pp.sessions_dir.ends_with("sessions"));
        assert!(pp.index_db.ends_with("index.db"));
    }

    #[test]
    fn owning_root_looks_through_a_pilot_worktree() {
        let tmp = std::env::temp_dir().join(format!("wingman-owning-{}", std::process::id()));
        let project = tmp.join("repo");
        let worktree = project.join(".wingman").join("worktrees").join("auto-x");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(project.join(".wingman")).unwrap();
        // A git worktree's `.git` is a FILE, which is why find_project_root
        // stops here — and why a session log written under it is deleted with
        // the worktree at cleanup.
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../../.git/worktrees/auto-x",
        )
        .unwrap();

        assert_eq!(find_project_root(&worktree), worktree, "containment root");
        assert_eq!(find_owning_project_root(&worktree), project, "owning root");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn owning_root_is_the_plain_root_outside_a_worktree() {
        let tmp = std::env::temp_dir().join(format!("wingman-owning2-{}", std::process::id()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".wingman")).unwrap();
        assert_eq!(find_owning_project_root(&nested), tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_project_root_finds_marker() {
        let tmp = std::env::temp_dir().join(format!("wingman-test-{}", std::process::id()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".wingman")).unwrap();
        let found = find_project_root(&nested);
        assert_eq!(found, tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /* ── WINGMAN_HOME ──────────────────────────────────────────────────── */

    fn v(s: &str) -> Result<Option<PathBuf>, (String, String)> {
        validate_home(Some(std::ffi::OsStr::new(s)))
    }

    #[test]
    fn an_absolute_override_is_taken_as_the_global_dir_itself() {
        // `CARGO_HOME` semantics: the value *is* the directory, with no
        // `.wingman` appended to it.
        let dir = if cfg!(windows) { r"C:\wm" } else { "/tmp/wm" };
        assert_eq!(v(dir).unwrap(), Some(PathBuf::from(dir)));
    }

    #[test]
    fn unset_and_empty_both_mean_the_real_home() {
        assert_eq!(validate_home(None).unwrap(), None);
        // `WINGMAN_HOME=$UNSET wingman …` — a shell spelling "unset" by
        // accident must not root the global directory at "".
        assert_eq!(v("").unwrap(), None);
        assert_eq!(v("   ").unwrap(), None);
    }

    #[test]
    fn a_relative_override_is_refused_rather_than_resolved() {
        // Silently binding this to the first cwd would be worse than refusing:
        // project resolution moves the process cwd, so the global dir would
        // depend on call order.
        let (value, reason) = v(".wingman-test").unwrap_err();
        assert_eq!(value, ".wingman-test");
        assert!(reason.contains("absolute"), "{reason}");
    }

    #[test]
    fn the_error_names_the_variable_so_it_can_be_found() {
        // It surfaces as a ConfigError, which is what the user actually reads.
        let e = ConfigError::BadEnv {
            name: HOME_ENV.to_string(),
            value: "rel".into(),
            reason: "must be an absolute path".into(),
        };
        let text = e.to_string();
        assert!(text.contains("WINGMAN_HOME"), "{text}");
        assert!(text.contains("absolute"), "{text}");
    }
}
