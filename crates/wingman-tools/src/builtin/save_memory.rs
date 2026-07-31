//! `save_memory`: persist a fact / preference / instruction the agent has
//! learned about the user or project. Backed by [`wingman_learn::MemoryStore`].

use std::sync::{Arc, Mutex};

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};
use wingman_learn::hooks::LearnSignals;
use wingman_learn::memory::{MemoryDraft, MemoryScope, MemoryStore, MemoryType};

pub struct SaveMemory {
    store: Arc<MemoryStore>,
    signals: Arc<Mutex<LearnSignals>>,
    /// From `[learn].allow_global_memory_writes`. Off by default — see the
    /// refusal message in `run` for why.
    allow_global: bool,
}

impl SaveMemory {
    pub fn new(store: Arc<MemoryStore>, signals: Arc<Mutex<LearnSignals>>) -> Self {
        Self {
            store,
            signals,
            allow_global: false,
        }
    }

    /// Permit the agent to write global (cross-project) memories.
    pub fn with_global_writes(mut self, allow: bool) -> Self {
        self.allow_global = allow;
        self
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    name: String,
    description: String,
    #[serde(rename = "type")]
    mtype: String,
    body: String,
    #[serde(default)]
    scope: Option<String>,
}

#[async_trait]
impl Tool for SaveMemory {
    fn capabilities(&self) -> Capability {
        Capability::WRITE
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "save_memory".into(),
            description: "Persist a fact, preference, or instruction about the user or project so \
                          future sessions can read it. Use this when the user says \"remember\", \
                          \"from now on\", or expresses a stable preference. \
                          Types: 'user' (about the human), 'feedback' (how to behave), \
                          'project' (about this codebase), 'reference' (pointer to external info). \
                          Scope defaults to 'global' for user/feedback/reference and 'project' for project."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":        { "type": "string", "description": "Short slug, e.g. 'prefers-terse'." },
                    "description": { "type": "string", "description": "One-line summary used in the prompt index." },
                    "type":        { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
                    "body":        { "type": "string", "description": "Full memory body in markdown." },
                    "scope":       { "type": "string", "enum": ["global", "project"], "description": "Override default scope." }
                },
                "required": ["name", "description", "type", "body"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let mtype = match MemoryType::parse(&args.mtype) {
            Some(t) => t,
            None => {
                return ToolOutcome::err(format!(
                    "unknown memory type '{}': expected one of user|feedback|project|reference",
                    args.mtype
                ))
            }
        };
        let scope = match args.scope.as_deref() {
            None => None,
            Some("global") => Some(MemoryScope::Global),
            Some("project") => Some(MemoryScope::Project),
            Some(other) => {
                return ToolOutcome::err(format!(
                    "unknown scope '{other}': expected 'global' or 'project'"
                ))
            }
        };

        // A global memory is rendered into the system prompt of every future
        // session in every project. That makes it a durable, cross-project
        // channel: one prompt injection in one cloned repo that induces a
        // save_memory call would follow the user into unrelated work
        // indefinitely. Project scope has no such reach, so it is the default
        // the agent may use freely.
        let effective = scope.unwrap_or_else(|| mtype.default_scope());
        if effective == MemoryScope::Global && !self.allow_global {
            return ToolOutcome::err(
                "refusing to write a GLOBAL memory: global memories are injected into                  every future session in every project. Save it with                  `scope: \"project\"` instead, or the user can enable                  `[learn].allow_global_memory_writes` if they want cross-project                  memories written automatically.",
            );
        }
        let draft = MemoryDraft {
            name: args.name.clone(),
            description: args.description,
            mtype,
            body: args.body,
            scope,
        };
        match self.store.save(draft) {
            Ok(path) => {
                if let Ok(mut s) = self.signals.lock() {
                    s.saved_this_session = true;
                }
                ToolOutcome::ok(format!(
                    "Saved memory '{}' to {}",
                    args.name,
                    path.display()
                ))
            }
            Err(e) => ToolOutcome::err(format!("save_memory: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_config::PermissionMode;

    fn store_in(tag: &str) -> (std::path::PathBuf, Arc<MemoryStore>) {
        let root = std::env::temp_dir().join(format!("wm-savemem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (root.clone(), Arc::new(MemoryStore::new(root)))
    }

    fn args(scope: Option<&str>, mtype: &str) -> serde_json::Value {
        let mut v = serde_json::json!({
            "name": "note",
            "description": "d",
            "type": mtype,
            "body": "b",
        });
        if let Some(s) = scope {
            v["scope"] = serde_json::json!(s);
        }
        v
    }

    /// A global memory reaches every future session in every project, so an
    /// injection that induces one is a durable cross-project compromise.
    #[tokio::test]
    async fn global_scope_is_refused_by_default() {
        let (root, store) = store_in("deny");
        let tool = SaveMemory::new(store, Arc::new(Mutex::new(LearnSignals::default())));
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());

        let out = tool.run(args(Some("global"), "project"), &ctx).await;
        assert!(out.is_error, "explicit global scope should be refused");
        assert!(out.content.contains("scope"));

        // `feedback` defaults to global, so it must be refused too — the
        // default is what an injection would actually reach for.
        let out = tool.run(args(None, "feedback"), &ctx).await;
        assert!(out.is_error, "default-global type should be refused");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn project_scope_is_always_allowed() {
        let (root, store) = store_in("allow");
        let tool = SaveMemory::new(store, Arc::new(Mutex::new(LearnSignals::default())));
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());

        let out = tool.run(args(Some("project"), "feedback"), &ctx).await;
        assert!(!out.is_error, "project scope must stay usable: {out:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn global_scope_allowed_when_the_user_opts_in() {
        let (root, store) = store_in("optin");
        let tool = SaveMemory::new(store, Arc::new(Mutex::new(LearnSignals::default())))
            .with_global_writes(true);
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());

        let out = tool.run(args(Some("global"), "user"), &ctx).await;
        assert!(!out.is_error, "opt-in should permit global writes: {out:?}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
