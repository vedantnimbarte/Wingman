//! Batch mode: read prompts from a JSONL file and run each one.
//!
//! Each line in the file should be `{"prompt": "..."}` or a bare string.
//! Results are printed as JSONL or plain text.

use anyhow::Result;
use wingman_config::Config;
use wingman_core::{AgentEvent, AgentStop};
use futures::StreamExt;
use std::process::ExitCode;

use crate::runtime::resolve_selection;

pub struct BatchOptions {
    pub file: String,
    pub json: bool,
    pub mode_override: Option<wingman_config::PermissionMode>,
    pub model_override: Option<String>,
}

pub async fn run(cfg: Config, opts: BatchOptions) -> Result<ExitCode> {
    let mode = opts.mode_override.unwrap_or(cfg.permission_mode);
    let sel = resolve_selection(&cfg, opts.model_override.as_deref())?;
    let (mut agent, registry) =
        crate::runtime::build_agent_registry_with_fallback(&cfg, &sel, mode).await?;
    // Seed MCP servers so `mcp__*` tools are available in batch mode too.
    let _mcp = crate::runtime::seed_mcp(&cfg, registry).await;

    let content = tokio::fs::read_to_string(&opts.file)
        .await
        .map_err(|e| anyhow::anyhow!("cannot read batch file '{}': {e}", opts.file))?;

    // Non-zero exit if any prompt errors, so batch runs can gate CI/scripts.
    let mut exit = ExitCode::SUCCESS;

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let prompt = if line.starts_with('{') {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => v
                    .get("prompt")
                    .and_then(|p| p.as_str())
                    .unwrap_or(line)
                    .to_string(),
                Err(_) => line.to_string(),
            }
        } else {
            // Bare quoted string
            if line.starts_with('"') {
                serde_json::from_str::<String>(line).unwrap_or_else(|_| line.to_string())
            } else {
                line.to_string()
            }
        };

        if opts.json {
            println!(
                "{}",
                serde_json::json!({"type": "prompt_start", "index": i, "prompt": prompt})
            );
        } else {
            eprintln!("\n--- prompt {} ---\n{}", i + 1, prompt);
        }

        let mut stream = agent.run(prompt);
        while let Some(event) = stream.next().await {
            // Any error on any prompt fails the whole batch, regardless of mode.
            match &event {
                AgentEvent::Error { .. }
                | AgentEvent::Stop {
                    reason: AgentStop::Error,
                } => exit = ExitCode::from(1),
                _ => {}
            }
            if opts.json {
                println!("{}", serde_json::to_string(&event).unwrap_or_default());
            } else {
                match &event {
                    AgentEvent::TextDelta { text } => {
                        print!("{text}");
                        use std::io::Write as _;
                        let _ = std::io::stdout().flush();
                    }
                    AgentEvent::ToolStart { name, .. } => eprint!("\n[tool: {name}] "),
                    AgentEvent::ToolResult { is_error: true, .. } => eprint!("[error] "),
                    AgentEvent::Stop { reason } if !matches!(reason, AgentStop::EndTurn) => {
                        eprintln!("\n(stop: {reason:?})");
                    }
                    AgentEvent::Error { message } => eprintln!("\nerror: {message}"),
                    _ => {}
                }
            }
            if matches!(event, AgentEvent::Stop { .. }) {
                break;
            }
        }
        if !opts.json {
            println!();
        }
    }

    Ok(exit)
}
