use std::collections::HashMap;
use std::sync::Arc;

use crate::{Tool, ToolCtx};
use arccode_core::{ToolDispatcher, ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    ctx: ToolCtx,
}

impl ToolRegistry {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            tools: HashMap::new(),
            ctx,
        }
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) -> &mut Self {
        let spec = tool.spec();
        self.tools.insert(spec.name, Arc::new(tool));
        self
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
        let mut out: Vec<ToolSpec> = self.tools.values().map(|t| t.spec()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    async fn dispatch(&self, name: &str, args: Value) -> ToolOutcome {
        match self.tools.get(name) {
            Some(tool) => tool.run(args, &self.ctx).await,
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        }
    }
}
