//! The [`LearnHook`] is the concrete `wingman_core::LearningHook` impl that
//! wires memory + stats + session-indexing into the agent loop.
//!
//! The trait itself lives in `wingman-core` (so the loop can call it without
//! a circular dep); this module is the production wiring.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use wingman_core::{ContentBlock, LearningHook, Message, Role};
use wingman_rag::Indexer;

use crate::{
    memory::{self, MemoryStore},
    proposal,
    stats::{looks_like_correction, Outcome, StatsStore},
};

/// Construction-time options.
#[derive(Debug, Clone)]
pub struct LearnConfig {
    pub project_root: PathBuf,
    /// Stable id for the current session (typically the session JSONL filename
    /// stem). Used as the session_id for stat rows.
    pub session_id: String,
    /// Disable everything if false. Defaults to true.
    pub enabled: bool,
    /// Threshold for the persistence nudge: how many sessions in a row can
    /// the user go without saving before we start nudging the agent.
    pub nudge_after_n_quiet: i64,
}

impl LearnConfig {
    pub fn new(project_root: PathBuf, session_id: String) -> Self {
        Self {
            project_root,
            session_id,
            enabled: true,
            nudge_after_n_quiet: proposal::NUDGE_AFTER_N_QUIET_SESSIONS,
        }
    }
}

/// Shared mutable bits that the tools touch directly (e.g. `save_memory`
/// records that a save happened so we can reset the quiet-session counter).
#[derive(Debug, Default)]
pub struct LearnSignals {
    pub saved_this_session: bool,
    /// Last unresolved skill-usage row id (for outcome scoring on the
    /// next user turn).
    pub pending_skill_row: Option<i64>,
}

pub struct LearnHook {
    cfg: LearnConfig,
    memory: Arc<MemoryStore>,
    stats: Arc<StatsStore>,
    signals: Arc<Mutex<LearnSignals>>,
    /// Cached count of recent user-message indexes we've already consumed
    /// so before_turn doesn't re-process the same turn for outcome scoring.
    last_user_idx: Mutex<usize>,
    /// Optional project index for proactive retrieval ("search escalation").
    /// `None` when RAG isn't wired (e.g. no embedder available).
    indexer: Option<Arc<Indexer>>,
    /// Cache of the retrieval block keyed by user-message count, so we only
    /// hit the index once per user turn and keep the injected context stable
    /// across the turn's tool round-trips.
    retrieval: Mutex<Option<(usize, Option<String>)>>,
}

impl LearnHook {
    pub fn new(cfg: LearnConfig, memory: Arc<MemoryStore>, stats: Arc<StatsStore>) -> Self {
        Self {
            cfg,
            memory,
            stats,
            signals: Arc::new(Mutex::new(LearnSignals::default())),
            last_user_idx: Mutex::new(0),
            indexer: None,
            retrieval: Mutex::new(None),
        }
    }

    /// Attach the project index so `before_turn` can inject relevant code
    /// locations for each request. Builder-style; a no-op when `idx` is None.
    pub fn with_indexer(mut self, idx: Option<Arc<Indexer>>) -> Self {
        self.indexer = idx;
        self
    }

    pub fn signals(&self) -> Arc<Mutex<LearnSignals>> {
        self.signals.clone()
    }

    pub fn memory(&self) -> Arc<MemoryStore> {
        self.memory.clone()
    }

    pub fn stats(&self) -> Arc<StatsStore> {
        self.stats.clone()
    }

    pub fn config(&self) -> &LearnConfig {
        &self.cfg
    }

    /// Examine the most recent user message and, if it follows a recent
    /// `invoke_skill` whose outcome is still 'unclear', flip it based on
    /// negation heuristics.
    fn score_pending_outcome(&self, history: &[Message]) {
        let mut signals = self.signals.lock().unwrap();
        let Some(row_id) = signals.pending_skill_row else {
            return;
        };
        // Find the latest user turn that arrived *after* the invoke (we
        // record the invoke at assistant turn k, so the next user turn is
        // the response we're judging).
        let last_user_text = history
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            });
        if let Some(text) = last_user_text {
            let outcome = match looks_like_correction(&text) {
                Some(sig) => {
                    let _ = self
                        .stats
                        .set_outcome(row_id, Outcome::Corrected, Some(sig));
                    Outcome::Corrected
                }
                None => {
                    let _ = self.stats.set_outcome(row_id, Outcome::Success, None);
                    Outcome::Success
                }
            };
            tracing::debug!(
                "scored skill row {row_id} as {} from user reply",
                outcome.as_str()
            );
            signals.pending_skill_row = None;
        }
    }
}

impl LearnHook {
    /// Latest user message flattened to plain text (concatenated text blocks).
    fn latest_user_text(history: &[Message]) -> Option<String> {
        let msg = history.iter().rev().find(|m| m.role == Role::User)?;
        let text: String = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Search escalation: pull the top index hits for the latest request and
    /// render their locations, so the agent starts from the right files
    /// instead of spending tool turns grepping. Returns `None` when there's
    /// no index, no useful query, or no hits.
    async fn retrieval_block(&self, history: &[Message]) -> Option<String> {
        let indexer = self.indexer.as_ref()?;
        let query = Self::latest_user_text(history)?;
        // Very short prompts ("yes", "go on") aren't concept queries.
        if query.trim().len() < 8 {
            return None;
        }
        let hits = match indexer.search(query.trim(), 4).await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("search-escalation retrieval failed: {e}");
                return None;
            }
        };
        if hits.is_empty() {
            return None;
        }
        let mut lines = vec![
            "Relevant code from the project index (semantic search on the latest request). \
             Prefer reading these locations over a fresh grep:"
                .to_string(),
        ];
        for h in &hits {
            let sym = h
                .symbol
                .as_deref()
                .map(|s| format!("  {s}"))
                .unwrap_or_default();
            lines.push(format!("- {}:{}-{}{}", h.path, h.start_line, h.end_line, sym));
        }
        Some(lines.join("\n"))
    }
}

#[async_trait]
impl LearningHook for LearnHook {
    async fn before_turn(&self, history: &[Message]) -> Option<String> {
        if !self.cfg.enabled {
            return None;
        }

        // Outcome scoring: every new user turn is a chance to resolve any
        // pending skill invocation.
        let current_user_count = history.iter().filter(|m| m.role == Role::User).count();
        {
            let mut idx = self.last_user_idx.lock().unwrap();
            if current_user_count > *idx {
                *idx = current_user_count;
                drop(idx);
                self.score_pending_outcome(history);
            }
        }

        let mut hints: Vec<String> = Vec::new();

        // Search escalation: inject top index hits for this user turn. Cached
        // by user-message count so we hit the index once per turn (the guard
        // is dropped before the await — std Mutex can't cross it).
        if self.indexer.is_some() {
            let cached = {
                let cache = self.retrieval.lock().unwrap();
                match &*cache {
                    Some((n, block)) if *n == current_user_count => Some(block.clone()),
                    _ => None,
                }
            };
            let block = match cached {
                Some(b) => b,
                None => {
                    let b = self.retrieval_block(history).await;
                    *self.retrieval.lock().unwrap() = Some((current_user_count, b.clone()));
                    b
                }
            };
            if let Some(b) = block {
                hints.push(b);
            }
        }

        // Quiet-session nudge.
        let quiet = self.stats.counter_get("sessions_without_save").unwrap_or(0);
        if quiet >= self.cfg.nudge_after_n_quiet {
            hints.push(proposal::nudge_line().to_string());
        }

        // Skill-rewrite suggestion: surface any skill that's been
        // repeatedly corrected so the agent can propose an edit.
        if let Ok(summary) = self.stats.summary() {
            let bad: Vec<String> = summary
                .into_iter()
                .filter(|s| s.needs_rewrite())
                .map(|s| s.skill_name)
                .collect();
            if !bad.is_empty() {
                hints.push(format!(
                    "Skill rewrite suggested: the following skills have been corrected by the \
                     user in >=50% of recent uses — consider proposing an improved body and \
                     writing it via `save_memory` (type=feedback) or `/skills new`: {}",
                    bad.join(", ")
                ));
            }
        }

        if hints.is_empty() {
            None
        } else {
            Some(hints.join("\n\n"))
        }
    }

    fn after_turn(&self, _history: &[Message]) {
        // Reserved for future use (e.g. live skill rewriting).
    }

    fn after_stop(&self, _history: &[Message]) {
        if !self.cfg.enabled {
            return;
        }
        let signals = self.signals.lock().unwrap();
        if signals.saved_this_session {
            let _ = self.stats.counter_set("sessions_without_save", 0);
        } else {
            let _ = self.stats.counter_incr("sessions_without_save");
        }
    }
}

/// Convenience builder used by the CLI to construct a fully wired hook +
/// the shared stores/signals the tools need.
pub struct LearnHandles {
    pub hook: Arc<LearnHook>,
    pub memory: Arc<MemoryStore>,
    pub stats: Arc<StatsStore>,
    pub signals: Arc<Mutex<LearnSignals>>,
}

impl LearnHandles {
    pub fn build(cfg: LearnConfig) -> crate::Result<Self> {
        Self::build_with_indexer(cfg, None)
    }

    /// Like [`build`], but attaches a project index so the hook can do
    /// proactive retrieval ("search escalation") each turn.
    pub fn build_with_indexer(
        cfg: LearnConfig,
        indexer: Option<Arc<Indexer>>,
    ) -> crate::Result<Self> {
        let memory = Arc::new(MemoryStore::new(cfg.project_root.clone()));
        memory.ensure_indexes()?;
        let stats = Arc::new(StatsStore::open_default()?);
        let hook = Arc::new(LearnHook::new(cfg, memory.clone(), stats.clone()).with_indexer(indexer));
        let signals = hook.signals();
        Ok(Self {
            hook,
            memory,
            stats,
            signals,
        })
    }
}

/// Returns the full memory prompt block to splice into the system prompt.
pub fn memory_prompt_block(store: &MemoryStore) -> Option<String> {
    let mems = store.load_all();
    memory::render_prompt_block(&mems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_rag::{Embedder, HashEmbedder, IndexStore};

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn latest_user_text_flattens_last_user_message() {
        let history = vec![user("first"), user("where do we parse symbols")];
        assert_eq!(
            LearnHook::latest_user_text(&history).as_deref(),
            Some("where do we parse symbols")
        );
        assert_eq!(LearnHook::latest_user_text(&[]), None);
    }

    #[tokio::test]
    async fn before_turn_injects_index_hits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("parser.rs"),
            "fn parse_symbols(src: &str) { /* tokenize and parse symbols */ }\n",
        )
        .unwrap();

        let embedder = Arc::new(HashEmbedder::new(64));
        let store = IndexStore::open(&root.join("index.db"), embedder.id(), embedder.dim()).unwrap();
        let indexer = Arc::new(Indexer::new(root.clone(), embedder, Arc::new(store)));
        indexer.reindex_repo().await.unwrap();

        let cfg = LearnConfig::new(root.clone(), "test-session".into());
        let hook = LearnHook::new(
            cfg,
            Arc::new(MemoryStore::new(root.clone())),
            Arc::new(StatsStore::open(&root.join("learn.db")).unwrap()),
        )
        .with_indexer(Some(indexer));

        let history = vec![user("where do we parse symbols in this project")];
        let out = hook.before_turn(&history).await.unwrap_or_default();
        assert!(
            out.contains("parser.rs"),
            "expected retrieval block to cite parser.rs, got: {out}"
        );
        assert!(out.contains("project index"), "got: {out}");
    }
}
