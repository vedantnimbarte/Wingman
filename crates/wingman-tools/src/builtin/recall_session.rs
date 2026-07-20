//! `recall_session`: semantic search over indexed past sessions
//! (cross-project, served from `~/.wingman/sessions.db`).

use std::sync::Arc;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_rag::{Embedder, IndexStore};

pub struct RecallSession {
    store: Arc<IndexStore>,
    embedder: Arc<dyn Embedder>,
}

impl RecallSession {
    pub fn new(store: Arc<IndexStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
}

#[async_trait]
impl Tool for RecallSession {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall_session".into(),
            description: "Search across all past wingman sessions (cross-project) for \
                          conversations relevant to `query`. Use for \"have we discussed this \
                          before\" or \"how did we fix X last time\" questions. Returns the \
                          most relevant transcript chunks with their session ids; use \
                          `read_session` to drill in."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20, "default": 5 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let limit = args.limit.unwrap_or(5).clamp(1, 20) as usize;
        let hits = match wingman_learn::session_index::search_sessions(
            &self.store,
            &*self.embedder,
            &args.query,
            limit,
        )
        .await
        {
            Ok(h) => h,
            Err(e) => return ToolOutcome::err(format!("recall_session: {e}")),
        };
        if hits.is_empty() {
            return ToolOutcome::ok(
                "(no matches — the session index may be empty; only sessions that completed \
                 after enabling the learning loop are indexed)",
            );
        }
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "[{}] session:{}  (score {:.3})\n{}\n\n",
                i + 1,
                h.session_id,
                h.score,
                truncate(&h.snippet, 1200)
            ));
        }
        ToolOutcome::ok(out.trim_end().to_string())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…(truncated; use read_session for the full transcript)");
    out
}
