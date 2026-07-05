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

fn home() -> Result<PathBuf, ConfigError> {
    Ok(directories::BaseDirs::new()
        .ok_or(ConfigError::NoHome)?
        .home_dir()
        .to_path_buf())
}

/// Returns `~/.wingman/`. Pure path computation — does **not** create.
pub fn global_dir() -> Result<PathBuf, ConfigError> {
    Ok(home()?.join(".wingman"))
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
pub fn find_project_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    if current.is_file() {
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        }
    }
    let home_dir = home().ok();
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
    fn find_project_root_finds_marker() {
        let tmp = std::env::temp_dir().join(format!("wingman-test-{}", std::process::id()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".wingman")).unwrap();
        let found = find_project_root(&nested);
        assert_eq!(found, tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
