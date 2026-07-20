use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use serde_json::Value;
use wingman_config::HooksConfig;
use wingman_core::{ToolDispatcher, ToolOutcome, ToolSpec};

pub struct ToolRegistry {
    /// Interior-mutable so MCP servers can be added/removed at runtime
    /// from behind an `Arc<ToolRegistry>` shared with the running agent.
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    ctx: ToolCtx,
    hooks: HooksConfig,
    /// Optional append-only audit log path. When set, each dispatch appends a
    /// JSONL record — an enterprise/compliance trail of what the agent did.
    audit: Option<std::path::PathBuf>,
    /// Redact high-confidence secret tokens in tool output before it reaches
    /// the model (data-exfiltration guard). Default on.
    redact_output: bool,
}

impl ToolRegistry {
    pub fn new(ctx: ToolCtx) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            ctx,
            hooks: HooksConfig::default(),
            audit: None,
            redact_output: true,
        }
    }

    /// Attach a hooks configuration. Returns `self` for builder-style chaining.
    pub fn with_hooks(mut self, hooks: HooksConfig) -> Self {
        self.hooks = hooks;
        self
    }

    /// Enable the append-only audit log at `path`. Builder-style.
    pub fn with_audit(mut self, path: Option<std::path::PathBuf>) -> Self {
        self.audit = path;
        self
    }

    /// Toggle high-confidence secret redaction of tool output. Builder-style.
    pub fn with_output_redaction(mut self, on: bool) -> Self {
        self.redact_output = on;
        self
    }

    /// Append one audit record for a dispatched tool call. Best-effort: a
    /// logging failure never blocks the tool. The input is truncated so the
    /// trail stays compact and doesn't balloon with large tool arguments.
    fn write_audit(&self, name: &str, args: &Value, is_error: bool) {
        let Some(path) = &self.audit else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // Redact secret-looking values so the compliance trail never itself
        // leaks credentials, then bound the size.
        let redacted = redact_secrets(args);
        let mut input = redacted.to_string();
        if input.len() > 400 {
            input.truncate(400);
            input.push('…');
        }
        let record = serde_json::json!({
            "ts_ms": ts,
            "tool": name,
            "input": input,
            "is_error": is_error,
        });
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{record}");
        }
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
            .unwrap_or_else(|e| e.into_inner())
            .insert(spec.name, tool)
    }

    /// Remove a tool by name. Idempotent.
    pub fn unregister(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
    }

    /// Tool names currently registered.
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    pub fn ctx(&self) -> &ToolCtx {
        &self.ctx
    }

    /// Switch the permission mode live. Takes `&self` (the mode lives behind
    /// an atomic in [`ToolCtx`]) so it works through the `Arc<ToolRegistry>`
    /// the running agent shares — the next tool call is gated by `mode`.
    pub fn set_mode(&self, mode: wingman_config::PermissionMode) {
        self.ctx.set_mode(mode);
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
            self.register(crate::builtin::WhoCalls);
        }
        // LSP-backed intelligence. Registered unconditionally — each tool
        // degrades gracefully (returns a "fall back to the tree-sitter tools"
        // note) when the user has no language server installed for the file.
        self.register(crate::builtin::LspDefinition);
        self.register(crate::builtin::LspReferences);
        self.register(crate::builtin::LspHover);
        self.register(crate::builtin::LspDiagnostics);
        self.register(crate::builtin::LspRename);
        self.register(crate::builtin::LspCodeAction);
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
        let guard = self.tools.read().unwrap_or_else(|e| e.into_inner());
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

        // Snapshot any files this tool is about to mutate, so /undo can
        // restore them. Captured before the edit; persisted only on success.
        let pres: Vec<_> = wingman_core::checkpoint::mutating_paths(name, &args)
            .iter()
            .map(|p| wingman_core::checkpoint::capture(&self.ctx.project_root, p))
            .collect();

        // Clone the Arc out of the lock before awaiting so we don't hold
        // a guard across an `.await` (std::sync guards aren't Send).
        let tool = self
            .tools
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        let mut outcome = match tool {
            Some(tool) => tool.run(args.clone(), &self.ctx).await,
            None => ToolOutcome::err(format!("unknown tool: {name}")),
        };

        // Data-exfiltration guard: strip high-confidence secret tokens from the
        // output before it reaches the model, so a credential the agent read
        // (e.g. from a .env) can't be echoed back or smuggled out.
        if self.redact_output {
            let (redacted, n) = redact_output_secrets(&outcome.content);
            if n > 0 {
                outcome.content = redacted;
            }
        }

        if !outcome.is_error && !pres.is_empty() {
            wingman_core::checkpoint::commit(&self.ctx.project_root, pres);
        }

        // Append to the compliance audit trail, if enabled.
        self.write_audit(name, &args, outcome.is_error);

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

/// Recursively redact secret-looking values in a JSON value: any object key
/// that looks credential-bearing (`key`, `token`, `secret`, `password`,
/// `authorization`, `api_key`, …) has its string value replaced with
/// `"[redacted]"`. Used so the audit trail can't itself leak credentials.
fn redact_secrets(v: &Value) -> Value {
    fn is_secret_key(k: &str) -> bool {
        let k = k.to_ascii_lowercase();
        const NEEDLES: &[&str] = &[
            "password",
            "passwd",
            "secret",
            "token",
            "api_key",
            "apikey",
            "authorization",
            "auth",
            "credential",
            "private_key",
            "access_key",
        ];
        // `key` alone is too broad (matches innocuous "key"), so require it to
        // be a suffix/compound (api_key, access_key) or one of the needles.
        NEEDLES.iter().any(|n| k.contains(n)) || k == "key" || k.ends_with("_key")
    }
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if is_secret_key(k) && val.is_string() {
                    out.insert(k.clone(), Value::String("[redacted]".into()));
                } else {
                    out.insert(k.clone(), redact_secrets(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_secrets).collect()),
        other => other.clone(),
    }
}

/// Redact high-confidence secret tokens in `text`, returning `(redacted, n)`.
/// Only matches unambiguous credential shapes so it doesn't mangle normal
/// output. Used to stop the agent surfacing/exfiltrating a secret it read.
pub fn redact_output_secrets(text: &str) -> (String, usize) {
    use regex::Regex;
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            r"sk-[A-Za-z0-9_-]{20,}",        // OpenAI / Anthropic-style
            r"gh[pousr]_[A-Za-z0-9]{30,}",   // GitHub tokens
            r"AKIA[0-9A-Z]{16}",             // AWS access key id
            r"xox[baprs]-[A-Za-z0-9-]{10,}", // Slack tokens
            r"AIza[0-9A-Za-z_-]{35}",        // Google API key
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", // JWT
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", // PEM keys
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    });
    let mut out = text.to_string();
    let mut n = 0usize;
    for re in patterns {
        let count = re.find_iter(&out).count();
        if count > 0 {
            n += count;
            out = re.replace_all(&out, "[redacted-secret]").into_owned();
        }
    }
    (out, n)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};
    use async_trait::async_trait;
    use wingman_config::PermissionMode;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({}),
            }
        }
        async fn run(&self, _args: Value, _ctx: &ToolCtx) -> ToolOutcome {
            ToolOutcome::ok("ok")
        }
    }

    #[tokio::test]
    async fn audit_log_records_each_dispatch() {
        let dir = std::env::temp_dir().join(format!("wm-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.log");
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx).with_audit(Some(log.clone()));
        reg.register(EchoTool);
        let _ = reg.dispatch("echo", serde_json::json!({ "x": 1 })).await;
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            body.contains("\"tool\":\"echo\""),
            "audit missing tool: {body}"
        );
        assert!(body.contains("\"is_error\":false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redacts_high_confidence_output_tokens() {
        let (out, n) = super::redact_output_secrets(
            "key=sk-abcdefghij0123456789ABCDEF and AKIAIOSFODNN7EXAMPLE plus normal text",
        );
        assert_eq!(n, 2);
        assert!(!out.contains("sk-abcdefghij"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(out.contains("normal text"));
        assert!(out.contains("[redacted-secret]"));
    }

    #[test]
    fn leaves_ordinary_output_untouched() {
        let text = "fn main() { let key = compute(); println!(\"{key}\"); }";
        let (out, n) = super::redact_output_secrets(text);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }

    #[test]
    fn redacts_secret_looking_keys() {
        let v = serde_json::json!({
            "path": "src/x.rs",
            "api_key": "sk-secret",
            "nested": { "token": "abc", "note": "keep" },
            "arr": [ { "password": "p" } ]
        });
        let r = super::redact_secrets(&v);
        assert_eq!(r["path"], "src/x.rs");
        assert_eq!(r["api_key"], "[redacted]");
        assert_eq!(r["nested"]["token"], "[redacted]");
        assert_eq!(r["nested"]["note"], "keep");
        assert_eq!(r["arr"][0]["password"], "[redacted]");
    }

    #[tokio::test]
    async fn audit_redacts_secrets_in_the_log() {
        let dir = std::env::temp_dir().join(format!("wm-audit-red-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.log");
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx).with_audit(Some(log.clone()));
        reg.register(EchoTool);
        let _ = reg
            .dispatch("echo", serde_json::json!({ "api_key": "sk-leak" }))
            .await;
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(
            !body.contains("sk-leak"),
            "secret leaked into audit log: {body}"
        );
        assert!(body.contains("[redacted]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_audit_when_disabled() {
        let dir = std::env::temp_dir().join(format!("wm-audit-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let mut reg = ToolRegistry::new(ctx); // no audit
        reg.register(EchoTool);
        let _ = reg.dispatch("echo", serde_json::json!({})).await;
        assert!(!dir.join("audit.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
