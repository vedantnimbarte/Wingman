//! The [`LearnHook`] is the concrete `wingman_core::LearningHook` impl that
//! wires memory + stats + session-indexing into the agent loop.
//!
//! The trait itself lives in `wingman-core` (so the loop can call it without
//! a circular dep); this module is the production wiring.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wingman_core::{ContentBlock, LearningHook, Message, Role};

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
}

impl LearnHook {
    pub fn new(cfg: LearnConfig, memory: Arc<MemoryStore>, stats: Arc<StatsStore>) -> Self {
        Self {
            cfg,
            memory,
            stats,
            signals: Arc::new(Mutex::new(LearnSignals::default())),
            last_user_idx: Mutex::new(0),
        }
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

impl LearningHook for LearnHook {
    fn before_turn(&self, history: &[Message]) -> Option<String> {
        if !self.cfg.enabled {
            return None;
        }

        // Outcome scoring: every new user turn is a chance to resolve any
        // pending skill invocation.
        let current_user_count = history.iter().filter(|m| m.role == Role::User).count();
        let mut idx = self.last_user_idx.lock().unwrap();
        if current_user_count > *idx {
            *idx = current_user_count;
            drop(idx);
            self.score_pending_outcome(history);
        }

        let mut hints: Vec<String> = Vec::new();

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
        let memory = Arc::new(MemoryStore::new(cfg.project_root.clone()));
        memory.ensure_indexes()?;
        let stats = Arc::new(StatsStore::open_default()?);
        let hook = Arc::new(LearnHook::new(cfg, memory.clone(), stats.clone()));
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
