# Embedding Wingman (Agent SDK)

Wingman is not just a CLI — its core is a library you can embed to build your own
agent, and it speaks MCP so you can drive it from any language. Two surfaces:

1. **Rust library** — depend on `wingman-core` (+ `wingman-providers`,
   `wingman-tools`) and drive the agent loop directly.
2. **Language-agnostic wire protocol** — run `wingman mcp-serve` and talk to it
   over MCP (JSON-RPC/stdio) from any language.

## 1. Rust library

The building blocks (`wingman-core`):

| Type | Role |
|---|---|
| `Provider` (trait) | An LLM backend. `wingman-providers` implements Anthropic, Gemini, OpenAI-compatible, Cohere, watsonx. |
| `ToolDispatcher` (trait) | Handles tool calls. `wingman_tools::ToolRegistry` implements it. |
| `TurnGate` (trait) | Post-edit verification (optional). |
| `LearningHook` (trait) | Per-turn hooks (memory recall, injection). `NoopLearningHook` for none. |
| `AgentConfig` | System prompt, model, token budgets, gate, hook. |
| `AgentLoop` | The agent. `run(prompt) -> Stream<AgentEvent>`. |
| `AgentEvent` | `TextDelta`, `ToolStart`, `ToolResult`, `Usage`, `Verification`, `Stop`, … |

Minimal embedding:

```rust
use futures::StreamExt;
use std::sync::Arc;
use wingman_core::{AgentConfig, AgentEvent, AgentLoop, NoopLearningHook};
use wingman_providers::AnthropicProvider;
use wingman_tools::{ToolCtx, ToolRegistry};
use wingman_config::PermissionMode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. A provider (any `impl Provider`).
    let provider = Arc::new(AnthropicProvider::new(std::env::var("ANTHROPIC_API_KEY")?)?);

    // 2. A tool registry (built-ins: read/write/edit/grep/shell/lsp/…).
    let cwd = std::env::current_dir()?;
    let ctx = ToolCtx::new(PermissionMode::ReadOnly, cwd.clone(), cwd);
    let tools = Arc::new(ToolRegistry::new(ctx).with_builtins());

    // 3. Configure and run the agent loop.
    let config = AgentConfig {
        model: "claude-sonnet-5".into(),
        system: "You are a helpful coding agent.".into(),
        ..AgentConfig::default()
    };
    let mut agent = AgentLoop::new(provider, tools, config, Arc::new(NoopLearningHook));

    let mut events = agent.run("List the files in this directory.".into());
    while let Some(ev) = events.next().await {
        match ev {
            AgentEvent::TextDelta { text } => print!("{text}"),
            AgentEvent::Stop { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
```

Bring your own pieces by implementing the traits:
- **Custom tool:** `impl wingman_tools::Tool` (a `spec()` + async `run()`), register it on the `ToolRegistry`.
- **Custom provider:** `impl wingman_core::Provider` to target a backend we don't ship.
- **Custom verification:** `impl wingman_core::TurnGate` and set it on `AgentConfig`.

> Exact field names for `AgentConfig` and constructor signatures are in
> `crates/wingman-core/src/agent.rs`; treat that as the source of truth (the API
> is pre-1.0 and may shift).

## 2. Language-agnostic (MCP)

`wingman mcp-serve` exposes Wingman's tools — including `semantic_search` (the
warm repo index) and `recall_memory` (team memory) — over MCP stdio. Any MCP
client library (Python, TypeScript, Go, …) can:

```jsonc
// → initialize, then:
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call",
 "params":{"name":"semantic_search","arguments":{"query":"agent loop"}}}
{"jsonrpc":"2.0","id":4,"method":"resources/read",
 "params":{"uri":"wingman-memory:///build-quirks"}}
```

This is the recommended integration path from non-Rust code: you get Wingman's
repo index + memory + LSP tools without linking Rust. See
[README](../README.md#wingman-as-an-mcp-server).

## Stability

Pre-1.0: the Rust API may change between minor versions. The MCP surface follows
the stable MCP spec and is the safer long-term integration point.
