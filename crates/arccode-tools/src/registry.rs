use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{Tool, ToolCtx};
use arccode_core::{ToolDispatcher, ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

pub struct ToolRegistry {
    /// Interior-mutable so MCP servers can be added/removed at runtime
    /// from behind an `Arc<ToolRegistry>` shared with the running agent.
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    ctx: ToolCtx,
}

impl ToolRegistry {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            ctx,
        }
    }

    /// Register a concrete tool at build time. Used by the chained-builder
    /// flow (`ToolRegistry::new(ctx).with_builtins()`) before the registry
    /// is wrapped in `Arc`.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        self.register_arc(Arc::new(tool));
        self
    }

    /// Insert a tool at runtime through shared (`&self`) access. Returns
    /// the previous tool with the same name, if any, so a caller can swap
    /// implementations without dropping live work.
    pub fn register_arc(&self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        let spec = tool.spec();
        self.tools
            .write()
            .expect("tools rwlock poisoned")
            .insert(spec.name, tool)
    }

    /// Remove a tool by name. Idempotent.
    pub fn unregister(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .write()
            .expect("tools rwlock poisoned")
            .remove(name)
    }

    /// Tool names currently registered.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .read()
            .expect("tools rwlock poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    pub fn with_builtins(mut self) -> Self {
        self.register(crate::builtin::ReadFile);
        self.register(crate::builtin::WriteFile);
        self.register(crate::builtin::EditFile);
        self.register(crate::builtin::RunShell);
        self.register(crate::builtin::ListDir);
        self.register(crate::builtin::Glob);
        self.register(crate::builtin::Grep);
        self
    }

    /// Register the `semantic_search` tool against a shared indexer. Call
    /// this on top of [`with_builtins`] when RAG is wired.
    pub fn with_semantic_search(mut self, indexer: std::sync::Arc<arccode_rag::Indexer>) -> Self {
        self.register(crate::builtin::SemanticSearch::new(indexer));
        self
    }
}

#[async_trait]
impl ToolDispatcher for ToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        let guard = self.tools.read().expect("tools rwlock poisoned");
        let mut out: Vec<ToolSpec> = guard.values().map(|t| t.spec()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        // Clone the Arc out of the lock before awaiting so we don't hold
        // a guard across an `.await` (std::sync guards aren't Send).
        let tool = self
            .tools
            .read()
            .expect("tools rwlock poisoned")
            .get(name)
            .cloned();
        match tool {
            Some(tool) => tool.run(args, &self.ctx).await,
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        }
    }
}
