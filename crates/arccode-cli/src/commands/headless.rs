//! Headless modes: `--print` for a single text-streaming response and
//! `--json` for newline-delimited structured events.

use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use arccode_config::{Config, PermissionMode, ProjectPaths};
use arccode_core::AgentEvent;
use arccode_session::{SessionLog, SessionRecord};
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
    let mut agent = runtime::build_agent(&cfg, &selection, mode).await?;

    // Open session log under the project's .arccode/sessions/ dir.
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
            "arccode [{}/{}] mode={mode}",
            selection.provider_id, selection.model
        );
    }

    let mut events = agent.run(opts.prompt);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let mut exit = ExitCode::SUCCESS;

    while let Some(event) = events.next().await {
        // Log to session.
        if let Some(s) = session.as_mut() {
            let _ = s.record_agent_event(&event).await;
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
