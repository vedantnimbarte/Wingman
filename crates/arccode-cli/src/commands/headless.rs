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
    /// Run the session in a freshly created detached git worktree and
    /// print the resulting diff at the end — the working tree the user
    /// invoked from is never touched.
    pub worktree: bool,
}

pub async fn run(cfg: Config, opts: HeadlessOptions) -> Result<ExitCode> {
    let dry_run_worktree = if opts.worktree {
        Some(enter_dry_run_worktree()?)
    } else {
        None
    };
    let mode = cfg.clamp_mode(opts.mode_override.unwrap_or(cfg.permission_mode));
    let selection = runtime::resolve_selection(&cfg, opts.model_override.as_deref())?;
    let mut agent = runtime::build_agent_with_fallback(&cfg, &selection, mode).await?;

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

    // Index the just-finished session into the global sessions store so
    // future runs can `recall_session` against it.
    if let Some(s) = session.as_ref() {
        let session_path = s.path().to_path_buf();
        tokio::spawn(async move {
            let embedder = crate::runtime::pick_embedder_pub();
            match arccode_learn::session_index::open_global_store(&*embedder) {
                Ok(store) => {
                    match arccode_learn::session_index::index_session_into(
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

    if let Some(wt) = dry_run_worktree {
        report_dry_run(&wt);
    }

    Ok(exit)
}

/// Create a detached worktree at HEAD under `.arccode/worktrees/` and make
/// it the process working directory, so every relative path the session
/// touches lands in the isolated copy.
fn enter_dry_run_worktree() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let wt_root = paths.dir.join("worktrees");
    std::fs::create_dir_all(&wt_root).ok();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = wt_root.join(format!("dryrun-{stamp}"));

    let out = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            dest.to_str().unwrap_or_default(),
            "HEAD",
        ])
        .current_dir(&paths.root)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "could not create dry-run worktree: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::env::set_current_dir(&dest)?;
    eprintln!("[dry-run] session sandboxed in {}", dest.display());
    Ok(dest)
}

/// Print the diff the session produced in the worktree and how to apply or
/// discard it. The worktree is left in place for inspection.
fn report_dry_run(wt: &std::path::Path) {
    let diff = std::process::Command::new("git")
        .args(["diff"])
        .current_dir(wt)
        .output();
    let status = std::process::Command::new("git")
        .args(["status", "--short"])
        .current_dir(wt)
        .output();

    eprintln!("\n[dry-run] proposed changes (sandbox: {}):", wt.display());
    let mut any = false;
    if let Ok(o) = status {
        let s = String::from_utf8_lossy(&o.stdout);
        if !s.trim().is_empty() {
            eprintln!("{}", s.trim_end());
            any = true;
        }
    }
    if let Ok(o) = diff {
        let s = String::from_utf8_lossy(&o.stdout);
        if !s.trim().is_empty() {
            println!("{s}");
            any = true;
        }
    }
    if !any {
        eprintln!("(no changes were made)");
    } else {
        eprintln!(
            "[dry-run] apply with:   git -C \"{}\" diff | git apply",
            wt.display()
        );
        eprintln!(
            "[dry-run] discard with: git worktree remove --force \"{}\"",
            wt.display()
        );
    }
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
