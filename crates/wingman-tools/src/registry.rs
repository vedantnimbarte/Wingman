use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{Tool, ToolCtx};
use wingman_config::HooksConfig;
use wingman_core::{ToolDispatcher, ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

pub struct ToolRegistry {
    /// Interior-mutable so MCP servers can be added/removed at runtime
    /// from behind an `Arc<ToolRegistry>` shared with the running agent.
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    ctx: ToolCtx,
    hooks: HooksConfig,
}

impl ToolRegistry {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            ctx,
            hooks: HooksConfig::default(),
        }
    }

    /// Attach a hooks configuration. Returns `self` for builder-style chaining.
    pub fn with_hooks(mut self, hooks: HooksConfig) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn hooks(&self) -> &HooksConfig {
        &self.hooks
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
        self.register(crate::builtin::ApplyPatch);
        self.register(crate::builtin::RunShell);
        self.register(crate::builtin::ListDir);
        self.register(crate::builtin::Glob);
        self.register(crate::builtin::Grep);
        self.register(crate::builtin::WebFetch);
        self.register(crate::builtin::WebSearch);
        self.register(crate::builtin::PresentPlan);
        self.register(crate::builtin::UpdateTasks);
        #[cfg(feature = "treesitter")]
        {
            self.register(crate::builtin::FindSymbol);
            self.register(crate::builtin::Outline);
            self.register(crate::builtin::EditSymbol);
        }
        self
    }

    /// Register the `semantic_search` tool against a shared indexer. Call
    /// this on top of [`with_builtins`] when RAG is wired.
    pub fn with_semantic_search(mut self, indexer: std::sync::Arc<wingman_rag::Indexer>) -> Self {
        self.register(crate::builtin::SemanticSearch::new(indexer));
        self
    }

    /// Register the `spawn_subagent` tool. The `runner` closure is supplied
    /// by the runtime (which knows how to build inner agents) so this
    /// crate stays provider-agnostic.
    pub fn with_subagent(mut self, runner: crate::builtin::SubagentRunner) -> Self {
        self.register(crate::builtin::SpawnSubagent::new(runner));
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
        // Pre-tool-use hooks: if a blocking hook fails, return a tool error.
        for hook in &self.hooks.pre_tool_use {
            if !tool_matches(name, &hook.match_tool) {
                continue;
            }
            let env = vec![
                ("WINGMAN_TOOL_NAME", name.to_string()),
                (
                    "WINGMAN_TOOL_INPUT",
                    serde_json::to_string(&args).unwrap_or_default(),
                ),
            ];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if hook.block && !res.success {
                return ToolOutcome::err(format!(
                    "pre_tool_use hook blocked: {}",
                    res.stderr.trim()
                ));
            }
        }

        // Clone the Arc out of the lock before awaiting so we don't hold
        // a guard across an `.await` (std::sync guards aren't Send).
        let tool = self
            .tools
            .read()
            .expect("tools rwlock poisoned")
            .get(name)
            .cloned();
        let outcome = match tool {
            Some(tool) => tool.run(args.clone(), &self.ctx).await,
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        };

        // Post-tool-use hooks: fire-and-forget; failures only logged.
        for hook in &self.hooks.post_tool_use {
            if !tool_matches(name, &hook.match_tool) {
                continue;
            }
            let env = vec![
                ("WINGMAN_TOOL_NAME", name.to_string()),
                (
                    "WINGMAN_TOOL_INPUT",
                    serde_json::to_string(&args).unwrap_or_default(),
                ),
                ("WINGMAN_TOOL_OUTPUT", outcome.content.clone()),
                ("WINGMAN_TOOL_IS_ERROR", outcome.is_error.to_string()),
            ];
            let res = run_hook(&hook.command, hook.timeout_secs, &env).await;
            if !res.success {
                tracing::warn!(
                    "post_tool_use hook failed (tool={}): {}",
                    name,
                    res.stderr.trim()
                );
            }
        }

        outcome
    }
}

fn tool_matches(name: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return true;
    }
    if let Some(rest) = pattern.strip_suffix('*') {
        name.starts_with(rest)
    } else if let Some(rest) = pattern.strip_prefix('*') {
        name.ends_with(rest)
    } else {
        name == pattern
    }
}

pub struct HookResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_hook(command: &str, timeout_secs: u64, env: &[(&str, String)]) -> HookResult {
    let command = command.to_string();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();

    let fut = tokio::task::spawn_blocking(move || {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", &command]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &command]);
            c
        };
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output()
    });

    let output = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await
    {
        Ok(Ok(Ok(o))) => o,
        Ok(Ok(Err(e))) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook spawn: {e}"),
            }
        }
        Ok(Err(e)) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook join: {e}"),
            }
        }
        Err(_) => {
            return HookResult {
                success: false,
                stdout: String::new(),
                stderr: format!("hook timed out after {timeout_secs}s"),
            }
        }
    };

    HookResult {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}
