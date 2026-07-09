//! Headless modes: `--print` for a single text-streaming response and
//! `--json` for newline-delimited structured events.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use wingman_config::{Config, PermissionMode, ProjectPaths};
use wingman_core::AgentEvent;
use wingman_session::{SessionLog, SessionRecord};
use futures::StreamExt;

use crate::runtime;

pub struct HeadlessOptions {
    pub prompt: String,
    pub json: bool,
    pub mode_override: Option<PermissionMode>,
    pub model_override: Option<String>,
}

pub async fn run(cfg: Config, opts: HeadlessOptions) -> Result<ExitCode> {
    let mode = opts.mode_override.unwrap_or(cfg.permission_mode);
    let selection = runtime::resolve_selection(&cfg, opts.model_override.as_deref())?;
    let (mut agent, registry) =
        runtime::build_agent_registry_with_fallback(&cfg, &selection, mode).await?;
    // Seed MCP servers so `mcp__*` tools are available in headless mode too.
    // Held for the whole run; dropping it tears down the server subprocesses.
    let _mcp = runtime::seed_mcp(&cfg, registry).await;

    // Open session log under the project's .wingman/sessions/ dir.
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let mut session = match SessionLog::create(&paths.sessions_dir).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("session log disabled: {e}");
            None
        }
    };
    if let Some(s) = session.as_mut() {
        let _ = s
            .write(SessionRecord::SessionStart {
                ts: chrono_rfc3339(),
                model: selection.model.clone(),
                provider: selection.provider_id.clone(),
                system_hash: None,
            })
            .await;
        let _ = s
            .write(SessionRecord::User {
                ts: chrono_rfc3339(),
                text: opts.prompt.clone(),
            })
            .await;
    }

    if !opts.json {
        eprintln!(
            "wingman [{}/{}] mode={mode}",
            selection.provider_id, selection.model
        );
    }

    let mut events = agent.run(opts.prompt);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let mut exit = ExitCode::SUCCESS;
    let mut assistant_text = String::new();

    while let Some(event) = events.next().await {
        // Log to session.
        if let Some(s) = session.as_mut() {
            let _ = s.record_agent_event(&event).await;
        }

        // Exit code + assistant-text capture, independent of output mode — a
        // mid-stream error or an error stop must fail the process in `--json`
        // mode too (previously it only did in the human-readable branch).
        match &event {
            AgentEvent::TextDelta { text } => assistant_text.push_str(text),
            AgentEvent::Error { .. } => exit = ExitCode::from(1),
            AgentEvent::Stop {
                reason: wingman_core::AgentStop::Error,
            } => exit = ExitCode::from(1),
            _ => {}
        }

        if opts.json {
            let line = serde_json::to_string(&event)
                .unwrap_or_else(|_| "{\"type\":\"serialize_error\"}".into());
            writeln!(stdout, "{line}").ok();
            stdout.flush().ok();
        } else {
            match &event {
                AgentEvent::TextDelta { text } => {
                    write!(stdout, "{text}").ok();
                    stdout.flush().ok();
                }
                AgentEvent::ToolStart { name, .. } => {
                    writeln!(stderr, "\n[tool] {name}…").ok();
                }
                AgentEvent::ToolResult { is_error, .. } => {
                    writeln!(
                        stderr,
                        "[tool done{}]",
                        if *is_error { " error" } else { "" }
                    )
                    .ok();
                }
                AgentEvent::Usage { usage } => {
                    writeln!(
                        stderr,
                        "[tokens] in={} out={} cache_read={} cache_creation={}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_input_tokens,
                        usage.cache_creation_input_tokens,
                    )
                    .ok();
                }
                AgentEvent::Verification { passed, summary } => {
                    let mark = if *passed { "✓" } else { "✗" };
                    writeln!(stderr, "\n[verify {mark}] {summary}").ok();
                }
                AgentEvent::TurnComplete => {}
                AgentEvent::Stop { .. } => {
                    writeln!(stdout).ok();
                }
                AgentEvent::Error { message } => {
                    writeln!(stderr, "\n[error] {message}").ok();
                    exit = ExitCode::from(1);
                }
            }
        }

        if matches!(event, AgentEvent::Stop { .. }) {
            break;
        }
    }

    // Persist the assistant's reply so the session isn't just a prompt with no
    // answer — recall_session and /resume both read this back.
    if let Some(s) = session.as_mut() {
        if !assistant_text.trim().is_empty() {
            let _ = s
                .record_message(&wingman_core::Message::assistant(vec![
                    wingman_core::ContentBlock::text(assistant_text),
                ]))
                .await;
        }
    }

    // Index the just-finished session into the global sessions store so
    // future runs can `recall_session` against it.
    if let Some(s) = session.as_ref() {
        let session_path = s.path().to_path_buf();
        tokio::spawn(async move {
            let embedder = crate::runtime::pick_embedder_pub();
            match wingman_learn::session_index::open_global_store(&*embedder) {
                Ok(store) => {
                    match wingman_learn::session_index::index_session_into(
                        &store,
                        &*embedder,
                        &session_path,
                    )
                    .await
                    {
                        Ok(n) => tracing::info!("indexed session ({n} chunks) into sessions.db"),
                        Err(e) => tracing::warn!("session indexing failed: {e}"),
                    }
                }
                Err(e) => tracing::warn!("could not open sessions store: {e}"),
            }
        });
    }

    Ok(exit)
}

fn chrono_rfc3339() -> String {
    // Minimal re-implementation to avoid a chrono dep in the CLI crate.
    // We just delegate to time-of-day via SystemTime; OK for log timestamps.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}
