//! `wingman --worker-mode` — pilot-mode worker subprocess.
//!
//! Invoked by the orchestrator (`wingman pilot`) once per task. The worker:
//!
//! 1. Reads its task spec from `--task-file <path>` (JSON of [`wingman_autonomous::Task`]).
//! 2. Loads the role's system prompt (`~/.wingman/agents/<role>.md` or the
//!    built-in default shipped with `wingman-autonomous`).
//! 3. Spins up the standard agent loop in `auto-edit` mode with the
//!    configured `pilot.worker_model`.
//! 4. Streams every `AgentEvent` to stdout as NDJSON — the parent
//!    supervisor parses each line.
//! 5. Registers the `task_complete` tool, which the worker is prompted to
//!    call exactly once before ending its turn. That tool prints a final
//!    `task_complete` NDJSON line and the supervisor uses it to decide
//!    success / failure.
//!
//! Cross-platform process control (Unix process groups, Windows Job
//! Objects) is the parent's concern — the worker itself is a plain process.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use wingman_autonomous::model::{Acceptance, Role, Task};
use wingman_autonomous::role::load_role_prompt_with_lessons;
use wingman_config::{Config, PermissionMode, ProjectPaths};
use wingman_core::{AgentConfig, AgentEvent, AgentLoop, Compactor, ToolOutputBudget};
use wingman_tools::{ToolCtx, ToolRegistry};
use futures::StreamExt;

use crate::runtime;

pub struct WorkerOptions {
    pub task_file: String,
    pub role: String,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub model_override: Option<String>,
}

pub async fn run(cfg: Config, opts: WorkerOptions) -> Result<ExitCode> {
    // Set cwd to the worktree, if one was passed. Relative paths inside
    // tool calls (edit_file, run_shell, etc.) then resolve against the
    // worker's isolated branch.
    if let Some(ref wt) = opts.worktree {
        std::env::set_current_dir(wt).with_context(|| format!("cd into worktree {wt}"))?;
    }

    // Parse the task spec.
    let task_json = std::fs::read_to_string(&opts.task_file)
        .with_context(|| format!("reading task file {}", opts.task_file))?;
    let task: Task = serde_json::from_str(&task_json)
        .with_context(|| format!("parsing task file {} as JSON", opts.task_file))?;
    let role = parse_role(&opts.role)?;

    // Resolve the worker model — prefer pilot.worker_model, then --model,
    // then the global default. We deliberately don't fall back to
    // pilot.default_model: workers should be the cheap tier.
    let model_string = cfg
        .pilot
        .worker_model
        .clone()
        .or_else(|| opts.model_override.clone())
        .or_else(|| cfg.default_model.clone());
    let selection = runtime::resolve_selection(&cfg, model_string.as_deref())?;
    let provider = runtime::build_provider(&cfg, &selection.provider_id)
        .with_context(|| format!("building provider {}", selection.provider_id))?;

    // Build a minimal tool registry: full builtins so the worker can read
    // / edit / run, plus our terminal `task_complete` tool. We bypass
    // `build_agent_and_registry` because we don't want the TUI-flavoured
    // system prompt; workers get a role-specific system prompt instead.
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let ctx = ToolCtx::new_with_config(
        PermissionMode::AutoEdit,
        cwd,
        paths.root.clone(),
        cfg.tools.shell_denylist.clone(),
    );
    let registry = ToolRegistry::new(ctx)
        .with_builtins()
        .with_hooks(cfg.hooks.clone());
    let registry = Arc::new(registry);
    registry.register_arc(Arc::new(wingman_tools::builtin::TaskComplete));
    registry.register_arc(Arc::new(wingman_autonomous::tools::RunAcceptance));

    let system = compose_worker_system_prompt(&role, &task);
    let user_prompt = compose_worker_user_prompt(&task);

    // E5.5 — per-turn sanity gate. When `[pilot].turn_gate_cmd` is set, the
    // worker's agent loop runs it after any turn that mutated files and feeds
    // failures back to the model (bounded by gate_max_retries) so it
    // self-corrects before reporting the task complete. Fail-open: a gate
    // that can't spawn passes. Empty cmd disables it.
    // ponytail: this is the "gate progress" half. True per-turn rollback of a
    // failed turn needs a checkpoint snapshot/restore primitive that doesn't
    // exist yet (E11 verifies checkpoints but never captures a restorable
    // one); until then the loop re-prompts rather than reverts.
    let gate: Option<Arc<dyn wingman_core::TurnGate>> = {
        let cmd = cfg.pilot.turn_gate_cmd.trim();
        if cmd.is_empty() {
            None
        } else {
            Some(Arc::new(runtime::ShellTurnGate::new(
                cmd.to_string(),
                paths.root.clone(),
            )))
        }
    };

    let agent_cfg = AgentConfig {
        model: selection.model.clone(),
        system: Some(system),
        tool_output_budget: ToolOutputBudget::new(cfg.tokens.tool_output_max_lines),
        compactor: Compactor {
            trigger_tokens: cfg.tokens.compact_at_tokens,
            ..Default::default()
        },
        gate,
        ..Default::default()
    };
    let mut agent = AgentLoop::new(provider, registry, agent_cfg);

    // E10 — read manager→worker IPC commands from stdin. A `cancel` sets a
    // shared flag the run loop checks so the manager can stop a live worker;
    // pivot/clarify are logged (mid-stream injection into the running agent
    // isn't supported by the loop API yet). The reader exits on EOF, which
    // the parent triggers by dropping the worker's stdin.
    // ponytail: cancel is the actionable command today; pivot/clarify need an
    // agent-loop message-injection hook that doesn't exist — add when it does.
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        // Blocking stdin read on a dedicated thread (avoids depending on
        // tokio's io-std feature). Exits on EOF when the parent drops stdin.
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                match wingman_autonomous::ipc::parse_command(line.trim()) {
                    Ok(wingman_autonomous::ipc::ManagerCommand::Cancel { reason }) => {
                        eprintln!("[worker] IPC cancel: {reason}");
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Ok(other) => eprintln!("[worker] IPC command (not injected): {other:?}"),
                    Err(_) => {}
                }
            }
        });
    }

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut exit = ExitCode::SUCCESS;

    // Emit a synthetic worker_start event so the supervisor can correlate
    // session-id, role, and the task without having to peek at the rest of
    // the stream.
    let start = serde_json::json!({
        "event": "worker_start",
        "task_id": task.id,
        "role": role.as_str(),
        "session_id": opts.session_id,
        "model": selection.model,
        "provider": selection.provider_id,
    });
    writeln!(stdout, "{start}").ok();
    stdout.flush().ok();

    let mut stream = agent.run(user_prompt);
    while let Some(event) = stream.next().await {
        let line = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"serialize_error\"}".into());
        writeln!(stdout, "{line}").ok();
        stdout.flush().ok();
        // E10 — honor a manager cancel between turns: emit a Blocked message
        // so the parent records why, then stop.
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            let msg = wingman_autonomous::ipc::encode_message(
                &wingman_autonomous::ipc::WorkerMessage::Blocked {
                    on: "cancelled by manager".into(),
                },
            );
            writeln!(stdout, "{msg}").ok();
            stdout.flush().ok();
            exit = ExitCode::from(1);
            break;
        }
        match event {
            AgentEvent::Error { .. } => {
                exit = ExitCode::from(1);
            }
            AgentEvent::Stop { .. } => break,
            _ => {}
        }
    }
    Ok(exit)
}

/// Compose the worker's system prompt: role prompt + the task spec, so the
/// model has everything it needs without further round-trips to the
/// orchestrator. The role markdown lays out hard rules; the task block
/// answers "what specifically am I doing?"
fn compose_worker_system_prompt(role: &Role, task: &Task) -> String {
    // E6 — fold this role's accumulated lessons (from prior reverted /
    // rewritten work) onto the base role prompt so the worker doesn't
    // repeat a mistake the same role already learned from.
    let mut s = load_role_prompt_with_lessons(role);
    s.push_str("\n\n# This task\n\n");
    s.push_str(&format!("- id: {}\n", task.id));
    s.push_str(&format!("- title: {}\n", task.title));
    if !task.goal.trim().is_empty() {
        s.push_str("\n## Goal\n");
        s.push_str(&task.goal);
        s.push('\n');
    }
    if !task.writes.is_empty() {
        s.push_str("\n## Allowed writes (do not edit other files unless necessary)\n");
        for w in &task.writes {
            s.push_str(&format!("- {w}\n"));
        }
    }
    if !task.acceptance.is_empty() {
        s.push_str("\n## Acceptance — run every check before reporting done\n");
        for a in &task.acceptance {
            s.push_str(&format!("- {}\n", render_acceptance(a)));
        }
    }
    s.push_str(
        "\n## When finished\n\nCommit your changes on this worktree, then call \
         `task_complete` with a one-paragraph summary and the list of files \
         changed. End your turn after that call — the orchestrator will pick \
         it up from there.\n\n\
         The moment your acceptance checks pass, call `task_complete` \
         immediately. Do not keep exploring — no further `glob`, `grep`, \
         `list_dir`, or `read_file` once acceptance is green. Calling \
         `task_complete` is mandatory: work you finish but never report this \
         way is thrown away, even if it is correct and committed.\n",
    );
    s
}

fn render_acceptance(a: &Acceptance) -> String {
    match a {
        Acceptance::Shell { cmd } => format!("shell: `{cmd}`"),
        Acceptance::Grep { pattern, path } => format!("grep: `{pattern}` in `{path}`"),
        Acceptance::Http { url, .. } => format!("http GET: `{url}`"),
        Acceptance::Run { target, .. } => format!("run: `{target}`"),
        Acceptance::Assert {
            screenshot,
            must_contain_text,
        } => {
            if must_contain_text.is_empty() {
                format!("assert rendered: `{screenshot}`")
            } else {
                format!(
                    "assert `{screenshot}` contains: {}",
                    must_contain_text.join(", ")
                )
            }
        }
    }
}

/// The user-turn prompt is intentionally terse — the system prompt already
/// carries the task. This lets the agent loop start straight into work
/// without the model wasting tokens restating what it already knows.
fn compose_worker_user_prompt(task: &Task) -> String {
    format!("Execute task `{}`: {}.", task.id, task.title)
}

fn parse_role(s: &str) -> Result<Role> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "developer" => Role::Developer,
        "designer" => Role::Designer,
        "tester" => Role::Tester,
        "reviewer" => Role::Reviewer,
        "refactorer" => Role::Refactorer,
        "merge-fixer" | "mergefixer" => Role::MergeFixer,
        other => {
            // Don't reject unknown roles — skill packs (J12) introduce new
            // ones at runtime. Just route to a Custom variant; the role
            // loader falls back to the developer default body.
            Role::Custom(other.to_string())
        }
    })
}
