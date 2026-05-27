//! Lifetime usage persistence — `~/.arccode/usage.json`.
//!
//! Format: a single JSON object mapping `"provider/model"` to a serialized
//! [`Usage`]. Missing file = empty map. Write failures are logged but never
//! propagated; usage tracking should never block the chat loop.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arccode_core::Usage;

const FILENAME: &str = "usage.json";

/// In-memory snapshot of the lifetime file as it was when we loaded it.
/// The session keeps its own delta map (in `StatusLine.usage`); the modal
/// renders "lifetime + session".
#[derive(Debug, Default, Clone)]
pub struct LifetimeUsage {
    pub totals: BTreeMap<String, Usage>,
}

impl LifetimeUsage {
    /// Load from `~/.arccode/usage.json`, returning empty on any error.
    pub fn load() -> Self {
        let Some(path) = file_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<BTreeMap<String, Usage>>(&text) {
                Ok(totals) => Self { totals },
                Err(e) => {
                    tracing::warn!("usage.json parse failed: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!("usage.json read failed: {e}");
                Self::default()
            }
        }
    }

    /// Atomically write `totals` merged with `session` deltas to disk.
    /// Errors are logged, not propagated.
    pub fn save_merged(&self, session: &BTreeMap<String, Usage>) {
        let Some(path) = file_path() else {
            return;
        };
        let mut merged = self.totals.clone();
        for (k, v) in session {
            merged.entry(k.clone()).or_default().add(v);
        }
        if let Err(e) = atomic_write_json(&path, &merged) {
            tracing::warn!("usage.json write failed: {e}");
        }
    }

    /// View used by the `/usage` modal's "Lifetime" tab: lifetime + session.
    pub fn combined(&self, session: &BTreeMap<String, Usage>) -> BTreeMap<String, Usage> {
        let mut out = self.totals.clone();
        for (k, v) in session {
            out.entry(k.clone()).or_default().add(v);
        }
        out
    }
}

fn file_path() -> Option<PathBuf> {
    arccode_config::ensure_global_dir()
        .ok()
        .map(|d| d.join(FILENAME))
}

fn atomic_write_json(path: &Path, data: &BTreeMap<String, Usage>) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
