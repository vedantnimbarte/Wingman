//! `wingman-lsp` — a Language Server Protocol client for real, resolved code
//! intelligence: go-to-definition, find-references, hover, rename, and live
//! diagnostics, backed by whatever servers the user has on `PATH`
//! (rust-analyzer, pyright / pylsp, typescript-language-server, gopls).
//!
//! This is the semantic upgrade over the tree-sitter heuristics in
//! `wingman-ts`/`who_calls`: those name-match; a language server *resolves*.
//! When a server isn't installed, callers degrade gracefully (the tools say so
//! and fall back to the heuristic tools) rather than failing.
//!
//! ## Shape
//! - [`server`] — which server backs each language, and `PATH` detection.
//! - [`client`] — one live server connection (JSON-RPC/stdio).
//! - [`LspManager`] — lazily starts and pools one client per language, per
//!   project root. Use [`manager_for`] to get the process-wide manager for a
//!   root so tool invocations share warm servers instead of respawning.
//! - [`edit`] — apply a server's `WorkspaceEdit` (used by `lsp_rename`).

pub mod client;
pub mod edit;
pub mod server;

pub use client::{Diagnostic, Location, LspClient, LspError, Position};
pub use server::{Lang, ServerSpec};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

/// Why an LSP operation couldn't run — surfaced to the agent so it knows to
/// fall back to the tree-sitter tools instead of treating this as a hard error.
#[derive(Debug, Clone)]
pub enum Unavailable {
    /// The file's language has no server we know how to launch.
    UnsupportedLanguage,
    /// A server exists for the language, but none of its candidates are on PATH.
    NoServerInstalled { lang: Lang, looked_for: String },
    /// The server was found but failed to start / handshake.
    StartFailed { lang: Lang, reason: String },
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::UnsupportedLanguage => {
                write!(f, "no language server is configured for this file type")
            }
            Unavailable::NoServerInstalled { lang, looked_for } => write!(
                f,
                "no {} language server on PATH (looked for: {looked_for})",
                lang.label()
            ),
            Unavailable::StartFailed { lang, reason } => {
                write!(
                    f,
                    "the {} language server failed to start: {reason}",
                    lang.label()
                )
            }
        }
    }
}

/// Lazily-started pool of one language-server client per language, scoped to a
/// single project root.
pub struct LspManager {
    root: PathBuf,
    // Per-language slot. `Some(Ok)` = live client; `Some(Err)` = known
    // unavailable (cached so we don't re-probe a missing server every call).
    clients: Mutex<HashMap<Lang, std::result::Result<Arc<LspClient>, Unavailable>>>,
}

impl LspManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LspManager {
            root: root.into(),
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get (or lazily start) the client that handles `path`'s language.
    pub async fn client_for_path(
        &self,
        path: &Path,
    ) -> std::result::Result<Arc<LspClient>, Unavailable> {
        let lang = Lang::from_path(path).ok_or(Unavailable::UnsupportedLanguage)?;
        let mut guard = self.clients.lock().await;
        if let Some(existing) = guard.get(&lang) {
            return existing.clone();
        }
        let outcome = self.start(lang).await;
        guard.insert(lang, outcome.clone());
        outcome
    }

    async fn start(&self, lang: Lang) -> std::result::Result<Arc<LspClient>, Unavailable> {
        let spec = ServerSpec::for_lang(lang);
        let Some((program, args)) = spec.detect() else {
            return Err(Unavailable::NoServerInstalled {
                lang,
                looked_for: spec.candidate_names(),
            });
        };
        match LspClient::start(&self.root, &program, &args, lang).await {
            Ok(c) => Ok(c),
            Err(e) => Err(Unavailable::StartFailed {
                lang,
                reason: e.to_string(),
            }),
        }
    }

    /// Shut down every started server. Call on session end.
    pub async fn shutdown_all(&self) {
        let mut guard = self.clients.lock().await;
        for (_, slot) in guard.drain() {
            if let Ok(client) = slot {
                client.shutdown().await;
            }
        }
    }
}

/// Process-wide registry of managers keyed by canonical project root, so every
/// tool call for the same project reuses warm servers instead of respawning.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<LspManager>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<LspManager>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The shared [`LspManager`] for `root`, created on first use.
pub async fn manager_for(root: &Path) -> Arc<LspManager> {
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut reg = registry().lock().await;
    if let Some(m) = reg.get(&key) {
        return m.clone();
    }
    let m = Arc::new(LspManager::new(key.clone()));
    reg.insert(key, m.clone());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_reports_unsupported_language() {
        let m = LspManager::new(std::env::temp_dir());
        let err = m
            .client_for_path(Path::new("notes.md"))
            .await
            .err()
            .expect("markdown has no language server");
        assert!(matches!(err, Unavailable::UnsupportedLanguage));
    }

    #[tokio::test]
    async fn registry_returns_same_manager_for_a_root() {
        let root = std::env::temp_dir();
        let a = manager_for(&root).await;
        let b = manager_for(&root).await;
        assert!(Arc::ptr_eq(&a, &b));
    }
}
