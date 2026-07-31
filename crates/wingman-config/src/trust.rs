//! Per-project config trust records.
//!
//! A project's `.wingman/config.toml` is attacker-controlled the moment you
//! clone someone else's repository. Most of its keys are harmless (which model
//! to use, TUI preferences, token budgets), but a few are not: `[hooks]` runs
//! shell commands, `[mcp]` spawns arbitrary binaries, `[[tools.custom]]`
//! registers shell-backed tools, and `[verify]` runs a check command. Honoring
//! those from an untrusted file turns `git clone` into code execution.
//!
//! So the project layer is split: safe keys always merge, executable keys merge
//! only when the user has explicitly trusted *that exact file content*. Trust is
//! recorded as a SHA-256 of the file bytes keyed by absolute path, so editing a
//! trusted config revokes trust until it is re-granted.
//!
//! The store lives at `~/.wingman/trusted.toml`:
//!
//! ```toml
//! ["/home/you/src/project/.wingman/config.toml"]
//! sha256 = "e3b0c442..."
//! ```

use crate::{global_dir, ConfigError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One recorded trust decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustEntry {
    /// Lowercase hex SHA-256 of the config file's bytes at the time of trust.
    pub sha256: String,
}

/// The whole store: absolute config path -> entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrustStore(pub BTreeMap<String, TrustEntry>);

/// Path to the trust store (`~/.wingman/trusted.toml`). Pure computation.
pub fn trust_store_path() -> Result<PathBuf, ConfigError> {
    Ok(global_dir()?.join("trusted.toml"))
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        // Infallible for a String sink.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Hash of the file at `path`, or `None` if it can't be read.
pub fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| hash_bytes(&b))
}

/// Canonical key for `path`. Falls back to the lexical path when the file
/// can't be canonicalized (e.g. it was just deleted), so lookups stay stable.
fn key_for(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Read the store. A missing or malformed file yields an empty store rather
/// than an error — an unreadable trust store must fail *closed* (nothing
/// trusted), never open.
pub fn load_store() -> TrustStore {
    let Ok(path) = trust_store_path() else {
        return TrustStore::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return TrustStore::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

fn save_store(store: &TrustStore) -> Result<(), ConfigError> {
    let path = trust_store_path()?;
    crate::ensure_global_dir()?;
    let text = toml::to_string_pretty(store)?;
    crate::write_private(&path, &text)
}

/// Is `config_path`'s *current content* trusted?
///
/// False when the file is absent, unreadable, unrecorded, or has changed since
/// it was trusted.
pub fn is_trusted(config_path: &Path) -> bool {
    let Some(current) = hash_file(config_path) else {
        return false;
    };
    let store = load_store();
    store
        .0
        .get(&key_for(config_path))
        .is_some_and(|e| e.sha256 == current)
}

/// Record trust for `config_path`'s current content. Returns the recorded hash.
pub fn trust(config_path: &Path) -> Result<String, ConfigError> {
    let hash = hash_file(config_path).ok_or_else(|| ConfigError::Io {
        path: config_path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot read project config to trust it",
        ),
    })?;
    let mut store = load_store();
    store.0.insert(
        key_for(config_path),
        TrustEntry {
            sha256: hash.clone(),
        },
    );
    save_store(&store)?;
    Ok(hash)
}

/// Drop any trust record for `config_path`. Idempotent.
pub fn untrust(config_path: &Path) -> Result<bool, ConfigError> {
    let mut store = load_store();
    let removed = store.0.remove(&key_for(config_path)).is_some();
    if removed {
        save_store(&store)?;
    }
    Ok(removed)
}

/// The hash recorded for `config_path`, if any — regardless of whether it
/// still matches the file. Lets callers distinguish "never trusted" from
/// "trusted, then edited", which are very different things to report.
pub fn recorded_hash(config_path: &Path) -> Option<String> {
    load_store()
        .0
        .get(&key_for(config_path))
        .map(|e| e.sha256.clone())
}

/// All recorded trust decisions, as (path, sha256) pairs.
pub fn list() -> Vec<(String, String)> {
    load_store()
        .0
        .into_iter()
        .map(|(k, v)| (k, v.sha256))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_hex() {
        let h = hash_bytes(b"");
        // SHA-256 of the empty string.
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_changes_with_content() {
        assert_ne!(hash_bytes(b"a"), hash_bytes(b"b"));
    }

    #[test]
    fn missing_file_is_never_trusted() {
        let p = std::path::Path::new("definitely-does-not-exist-9f3a.toml");
        assert!(!is_trusted(p));
        assert_eq!(hash_file(p), None);
    }
}
