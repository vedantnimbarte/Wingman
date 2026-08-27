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
use futures::StreamExt;
use wingman_autonomous::model::{Acceptance, Role, Task};
use wingman_autonomous::role::load_role_prompt_with_lessons;
use wingman_config::{Config, PermissionMode, ProjectPaths};
use wingman_core::{AgentConfig, AgentEvent, AgentLoop, Compactor, ToolOutputBudget};
use wingman_tools::ToolCtx;

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

    // The worker gets the *same* registry the interactive session gets, plus
    // its two control tools. It used to build one by hand, so everything
    // `base_registry` applies was simply absent here:
    //
    //   - the audit trail (none: unattended runs left no compliance record)
    //   - the per-call deadline (none: a wedged tool hung the worker)
    //   - the repeat guard (off)
    //   - custom tools from `[tools.custom]`
    //   - the `local_only` network-tool removal
    //   - `[tools].preset` / `[tools].disabled_tools`
    //   - `[tools].shell_sandbox`, which the ctx defaulted to "auto" — so a
    //     configured `required` ran commands unconfined instead of refusing
    //
    // (Secret redaction was already on, because the registry defaults it on;
    // what changes is that it now follows the config knob like everywhere
    // else.)
    //
    // This is the same drift that once hit `spawn_subagent`, and it matters
    // more here: "not this tool, ever" and "this box is air-gapped" were being
    // ignored precisely where nobody is watching. One builder is what stops it
    // happening a third time.
    //
    // The system prompt stays the worker's own — that is what this function
    // legitimately does differently, and it is not the registry's concern.
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let ctx = ToolCtx::new_with_config(
        PermissionMode::AutoEdit,
        cwd,
        paths.root.clone(),
        cfg.tools.shell_denylist.clone(),
        cfg.tools.allow_network,
    )
    .with_shell_sandbox(cfg.tools.shell_sandbox.clone());
    let mut registry = runtime::base_registry(ctx, &cfg, runtime::audit_path_for(&cfg, &paths));
    runtime::apply_tool_removals(&mut registry, &cfg);
    let registry = Arc::new(registry);
    registry.register_arc(Arc::new(wingman_tools::builtin::TaskComplete));
    registry.register_arc(Arc::new(wingman_autonomous::tools::RunAcceptance));

    // The removals now bind these two as well, and a worker without them
    // cannot report its result — it would run the whole task and then fail in
    // a way that looks like a model problem. Say so up front instead.
    for required in ["task_complete", "run_acceptance"] {
        if !registry.tool_names().iter().any(|n| n == required) {
            anyhow::bail!(
                "`{required}` is excluded by [tools].disabled_tools or [tools].preset, but the                  pilot worker cannot report a result without it. Remove it from that list, or                  narrow the setting to the tools you meant."
            );
        }
    }

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

    // E10 — mid-run manager→worker injections (pivot / clarify). The stdin
    // reader below pushes formatted messages here; the learning hook drains
    // them into the next turn's system prompt.
    let ipc_injections: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // Set when the operator asked a question (`pilot ask`): the next
    // assistant message is echoed back up the pipe as the answer.
    let answer_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let agent_cfg = AgentConfig {
        model: selection.model.clone(),
        system: Some(system),
        tool_output_budget: ToolOutputBudget::new(cfg.effective_tool_output_max_lines()),
        compactor: Compactor {
            trigger_tokens: cfg.tokens.compact_at_tokens,
            ..Default::default()
        },
        gate,
        // Not the interactive default: a worker has to read, edit, build,
        // read errors, fix, re-build and only then report. Sixteen turns ran
        // out mid-task and the worker exited cleanly without calling
        // `task_complete`, which the supervisor could only read as failure.
        max_turns: cfg.pilot.worker_max_turns,
        learning: Some(std::sync::Arc::new(IpcInjector {
            pending: ipc_injections.clone(),
        })),
        ..Default::default()
    };
    let mut agent = AgentLoop::new(provider, registry, agent_cfg);

    // Open the worker's own transcript.
    //
    // The orchestrator mints a session id and passes `--session-id`, and
    // `AgentRecord.session_id` is documented as "the worker's own JSONL log
    // under `<project>/.wingman/sessions/`. Lets `wingman session fork`
    // operate on any worker's transcript." No worker ever opened one, so that
    // id named a file that did not exist and a worker's turns could not be
    // forked, resumed, or recalled.
    //
    // Deliberately the OWNING project, not `paths.root`. A worker `cd`s into
    // `<project>/.wingman/worktrees/<name>` first, and a git worktree has a
    // `.git` file — so `paths.root` is the worktree, which is force-removed at
    // cleanup. A log written there would be deleted along with the evidence of
    // what the worker did.
    let sessions_dir = wingman_config::find_owning_project_root(
        &std::env::current_dir().unwrap_or_else(|_| paths.root.clone()),
    )
    .join(".wingman")
    .join("sessions");
    let opened = match opts.session_id.as_deref() {
        Some(id) => wingman_session::SessionLog::open_named(&sessions_dir, id).await,
        // The flag is optional. A worker started by hand still gets a
        // transcript; it just gets a timestamped name.
        None => wingman_session::SessionLog::create(&sessions_dir).await,
    };
    let session = match opened {
        Ok(mut log) => {
            let _ = log
                .write(wingman_session::SessionRecord::SessionStart {
                    ts: chrono::Utc::now().to_rfc3339(),
                    model: selection.model.clone(),
                    provider: selection.provider_id.clone(),
                    system_hash: agent.system_prompt().map(wingman_session::system_hash),
                })
                .await;
            Some(std::sync::Arc::new(wingman_session::SessionLogSink::new(
                log,
            )))
        }
        Err(e) => {
            // Best-effort: a worker that cannot write its transcript still
            // does the task. The supervisor reads the NDJSON stream, not this.
            eprintln!("[worker] session log disabled: {e}");
            None
        }
    };
    if let Some(sink) = &session {
        agent.set_context_sink(sink.clone());
    }

    // E10 — read manager→worker IPC commands from stdin. `cancel` sets a
    // shared flag the run loop checks; `pivot`/`clarify` are queued into
    // `ipc_injections` and the learning hook splices them into the next
    // turn's system prompt. The reader exits on EOF (parent drops stdin).
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        // Blocking stdin read on a dedicated thread (avoids depending on
        // tokio's io-std feature). Exits on EOF when the parent drops stdin.
        let cancel = cancel.clone();
        let injections = ipc_injections.clone();
        let answer_pending = answer_pending.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            use wingman_autonomous::ipc::ManagerCommand;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                match wingman_autonomous::ipc::parse_command(line.trim()) {
                    Ok(ManagerCommand::Cancel { reason }) => {
                        eprintln!("[worker] IPC cancel: {reason}");
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Ok(ManagerCommand::Pivot { goal, context }) => {
                        eprintln!("[worker] IPC pivot injected");
                        injections.lock().unwrap().push(format!(
                            "## Manager update — the task has PIVOTED\n\nNew goal: {goal}\n\n{context}\n\
                             Adjust your remaining work to this; do not redo what is already correct.",
                        ));
                    }
                    Ok(ManagerCommand::Clarify { answer }) => {
                        eprintln!("[worker] IPC clarify injected");
                        injections
                            .lock()
                            .unwrap()
                            .push(format!("## Manager clarification\n\n{answer}"));
                    }
                    Ok(ManagerCommand::Note { text, reply }) => {
                        eprintln!("[worker] IPC operator note injected (reply: {reply})");
                        // Deliberately not Pivot's wording: an operator adding
                        // a constraint has not changed what the task is, and
                        // telling the model it pivoted makes it redo work.
                        let framing = if reply {
                            "## Question from the human running this task\n\nAnswer it in your next message, briefly, then carry on with the task."
                        } else {
                            "## Message from the human running this task\n\nFold this into the work you have left; it does not mean the task changed."
                        };
                        injections
                            .lock()
                            .unwrap()
                            .push(format!("{framing}\n\n{text}"));
                        if reply {
                            answer_pending.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
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
    // Accumulates the assistant text of the message that answers an operator
    // question; flushed as a `WorkerMessage::Answer` when that message ends.
    let mut answer_buf = String::new();
    while let Some(event) = stream.next().await {
        let line = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"serialize_error\"}".into());
        writeln!(stdout, "{line}").ok();
        stdout.flush().ok();
        // `pilot ask`: mirror the reply back up the pipe so the asking process
        // has something to print. The message is over when the model stops
        // talking and does something else (a tool call, or the turn ending).
        if answer_pending.load(std::sync::atomic::Ordering::SeqCst) {
            match &event {
                AgentEvent::TextDelta { text } => answer_buf.push_str(text),
                AgentEvent::ToolStart { .. } | AgentEvent::Stop { .. }
                    if !answer_buf.trim().is_empty() =>
                {
                    let msg = wingman_autonomous::ipc::encode_message(
                        &wingman_autonomous::ipc::WorkerMessage::Answer {
                            text: answer_buf.trim().to_string(),
                        },
                    );
                    writeln!(stdout, "{msg}").ok();
                    stdout.flush().ok();
                    answer_buf.clear();
                    answer_pending.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                _ => {}
            }
        }
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
    // Queue the transcript for indexing, the same way headless does — a
    // worker's log that no `recall_session` can find is only half a record.
    // Appending to the queue is cheap; a later session drains it.
    if let Some(sink) = session.as_ref() {
        if let Err(e) = wingman_learn::session_index::enqueue_pending(sink.path()) {
            eprintln!("[worker] could not queue session for indexing: {e}");
        }
    }

    Ok(exit)
}

/// Compose the worker's system prompt: role prompt + the task spec, so the
/// model has everything it needs without further round-trips to the
/// orchestrator. The role markdown lays out hard rules; the task block
/// answers "what specifically am I doing?"
/// E10 learning hook that drains queued manager IPC messages (pivot /
/// clarify) into the next turn's system prompt, so a live run can be steered
/// mid-flight. The stdin reader fills `pending`; `before_turn` empties it.
struct IpcInjector {
    pending: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl wingman_core::LearningHook for IpcInjector {
    async fn before_turn(&self, _history: &[wingman_core::Message]) -> Option<String> {
        let mut q = self.pending.lock().unwrap();
        if q.is_empty() {
            return None;
        }
        Some(q.drain(..).collect::<Vec<_>>().join("\n\n"))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_core::LearningHook;

    /// The worker is the unattended path, so `[tools].disabled_tools` matters
    /// more there, not less. It used to build its registry by hand and never
    /// apply removals at all, so naming a tool did nothing — the same shape of
    /// bug as `spawn_subagent`, but with nobody watching the session.
    ///
    /// Pinned against `runtime::base_registry` + `apply_tool_removals`, which
    /// is the fix: one builder, so this cannot drift apart again.
    #[test]
    fn the_worker_registry_honors_disabled_tools() {
        let mut cfg = Config::default();
        cfg.tools.disabled_tools = vec!["run_shell".into()];

        let tmp = std::env::temp_dir();
        let ctx = ToolCtx::new_with_config(
            PermissionMode::AutoEdit,
            tmp.clone(),
            tmp,
            cfg.tools.shell_denylist.clone(),
            cfg.tools.allow_network,
        );
        let mut reg = runtime::base_registry(ctx, &cfg, None);
        runtime::apply_tool_removals(&mut reg, &cfg);

        assert!(
            !reg.tool_names().iter().any(|n| n == "run_shell"),
            "disabled_tools was ignored in the worker path: {:?}",
            reg.tool_names()
        );
        // The rest of the toolset is untouched — this is a denylist, not a
        // reason to start the worker with nothing.
        assert!(reg.tool_names().iter().any(|n| n == "read_file"));
    }

    /// A worker that cannot call `task_complete` runs the whole task and then
    /// fails in a way that reads as a model problem. Now that removals bind
    /// the control tools too, that has to be caught up front.
    #[test]
    fn removing_a_control_tool_is_detectable_before_the_run() {
        let mut cfg = Config::default();
        cfg.tools.disabled_tools = vec!["task_complete".into()];

        let tmp = std::env::temp_dir();
        let ctx = ToolCtx::new_with_config(
            PermissionMode::AutoEdit,
            tmp.clone(),
            tmp,
            cfg.tools.shell_denylist.clone(),
            cfg.tools.allow_network,
        );
        let mut reg = runtime::base_registry(ctx, &cfg, None);
        runtime::apply_tool_removals(&mut reg, &cfg);
        let reg = std::sync::Arc::new(reg);
        reg.register_arc(std::sync::Arc::new(wingman_tools::builtin::TaskComplete));

        // The registry refuses it, which is what `run` turns into a startup
        // error rather than a mystery at the end of the task.
        assert!(
            !reg.tool_names().iter().any(|n| n == "task_complete"),
            "an excluded control tool was registered anyway"
        );
    }

    #[tokio::test]
    async fn ipc_injector_drains_pending_once() {
        let pending = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook = IpcInjector {
            pending: pending.clone(),
        };
        // Empty queue → nothing to inject. (`before_turn` is async since the
        // search-escalation hook does I/O; await it here.)
        assert!(hook.before_turn(&[]).await.is_none());
        // Two queued messages are joined and injected once...
        pending.lock().unwrap().push("clarify: use tabs".into());
        pending.lock().unwrap().push("pivot: target v2".into());
        let injected = hook.before_turn(&[]).await.expect("injects pending");
        assert!(injected.contains("use tabs") && injected.contains("target v2"));
        // ...then drained, so the next turn injects nothing.
        assert!(hook.before_turn(&[]).await.is_none());
    }

    /// The regression this fixes: the orchestrator hands the worker a session
    /// id and documents it as the worker's log under
    /// `<project>/.wingman/sessions/`, so `wingman session fork` can target a
    /// worker's turns. The worker never opened one, and — because it `cd`s
    /// into a git worktree first — the obvious fix would have written it
    /// somewhere cleanup deletes.
    #[tokio::test]
    async fn a_workers_log_lands_in_the_project_not_the_worktree() {
        let tmp = std::env::temp_dir().join(format!(
            "wingman-worker-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = tmp.join("repo");
        let worktree = project.join(".wingman").join("worktrees").join("auto-x");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(project.join(".wingman")).unwrap();
        // A git worktree's `.git` is a file, which is what makes
        // `find_project_root` stop inside the worktree.
        std::fs::write(worktree.join(".git"), "gitdir: ../../../.git").unwrap();

        // The path the worker computes, from inside its worktree.
        let sessions_dir = wingman_config::find_owning_project_root(&worktree)
            .join(".wingman")
            .join("sessions");
        assert_eq!(
            sessions_dir,
            project.join(".wingman").join("sessions"),
            "the transcript must outlive the worktree it was produced in"
        );
        assert!(
            !sessions_dir.starts_with(&worktree),
            "a log under the worktree is force-removed at cleanup"
        );

        // And the id the orchestrator passed names a real, findable file.
        let log = wingman_session::SessionLog::open_named(&sessions_dir, "sess-1")
            .await
            .expect("worker should be able to open its named log");
        drop(log);
        assert!(
            wingman_session::session_path(&sessions_dir, "sess-1").is_some(),
            "`wingman session fork sess-1` must be able to find it"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
