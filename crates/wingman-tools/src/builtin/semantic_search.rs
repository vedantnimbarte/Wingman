//! `semantic_search`: query the per-project RAG index for relevant code
//! chunks. Returns ranked `path:start-end\nsnippet` blocks so the model
//! can follow up with a targeted `read_file` instead of reading whole
//! files.

use std::sync::Arc;

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_rag::Indexer;

pub struct SemanticSearch {
    indexer: Arc<Indexer>,
}

impl SemanticSearch {
    pub fn new(indexer: Arc<Indexer>) -> Self {
        Self { indexer }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    /// Cap on snippet body length per result (default 800 chars).
    #[serde(default)]
    snippet_chars: Option<u32>,
}

#[async_trait]
impl Tool for SemanticSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "semantic_search".into(),
            description: "Search the repo by semantic similarity to `query` and return ranked \
                          code chunks. Use this before `read_file` to find which file(s) are \
                          relevant — then follow up with `read_file` using the returned line range \
                          for the part you actually need."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 8 },
                    "snippet_chars": { "type": "integer", "minimum": 100, "maximum": 4000, "default": 800 }
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
        let limit = args.limit.unwrap_or(8).clamp(1, 50) as usize;
        let cap = args.snippet_chars.unwrap_or(800).clamp(100, 4000) as usize;

        let hits = match self.indexer.search(&args.query, limit).await {
            Ok(h) => h,
            Err(e) => return ToolOutcome::err(format!("search: {e}")),
        };

        if hits.is_empty() {
            return ToolOutcome::ok(
                "(no matches — the index may be empty; try `glob` or `grep` instead)",
            );
        }

        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            let snippet = truncate(&h.content, cap);
            let sym = match &h.symbol {
                Some(s) => format!("  [{s}]"),
                None => String::new(),
            };
            out.push_str(&format!(
                "[{}] {}:{}-{}  (score {:.3}){}\n{}\n\n",
                i + 1,
                h.path,
                h.start_line,
                h.end_line,
                h.score,
                sym,
                snippet,
            ));
        }
        ToolOutcome::ok(out.trim_end().to_string())
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("\n…(truncated, use read_file for the full chunk)");
    out
}
