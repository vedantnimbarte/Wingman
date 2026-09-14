//! Phase 8.4 — live provider-validation matrix.
//!
//! `wingman pilot validate-providers` runs one canned plan (add a
//! `--version-only` flag to a throwaway CLI) against each configured
//! provider, each in a scratch git repo of its own and under a strict spend
//! cap, and records whether the run reached a merged integration branch that
//! carries the flag. That exercises the provider's tool-call shape end to end:
//! the manager has to call `assign_task` and `finalize_task`, the worker has to
//! edit a file, run its acceptance check and call `task_complete`.
//!
//! The planner is skipped on purpose. A plan the model writes differs per
//! provider, so a failed row could not be told apart from a bad plan.
//!
//! This module owns the scratch repo, the plan, the verdict and the report.
//! The CLI builds the real provider and worker spawner; tests hand in a
//! scripted provider instead.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::model::{Acceptance, Role, RunStatus, TaskStatus};
use crate::pipeline::PipelineInputs;
use crate::planner::PlannedTask;
use crate::store::RunStore;

/// Goal recorded on every validation run.
pub const GOAL: &str = "add a --version-only flag to the demo CLI";
/// Run id inside each scratch repo. One run per repo, so it never collides.
pub const RUN_ID: &str = "provider-validation";

const FLAG: &str = "--version-only";
const MAIN_RS: &str = "src/main.rs";
const SEED_MAIN: &str = r#"const VERSION: &str = "0.1.0";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("demo {VERSION}");
        return;
    }
    println!("usage: demo [--version]");
}
"#;

/// The one-task plan every provider runs.
pub fn canned_plan() -> Vec<PlannedTask> {
    vec![PlannedTask {
        id: "t1".into(),
        role: Role::Developer,
        title: "Add a --version-only flag".into(),
        goal: format!(
            "In `{MAIN_RS}`, add a `{FLAG}` flag that prints only the version \
             (`0.1.0`, without the program name) and exits. Keep `--version` as \
             it is and list the new flag in the usage line. This repo has no \
             build system: do not try to compile it, and do not add files."
        ),
        deps: Vec::new(),
        writes: vec![MAIN_RS.into()],
        acceptance: vec![Acceptance::Grep {
            pattern: FLAG.into(),
            path: MAIN_RS.into(),
        }],
        reversibility: Default::default(),
        reversibility_reason: None,
    }]
}

/// Write the demo CLI into `dir`, commit it, and return the commit.
pub fn init_scratch_repo(dir: &Path) -> std::io::Result<String> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join(MAIN_RS), SEED_MAIN)?;
    std::fs::write(dir.join(".gitignore"), ".wingman/\n")?;
    git(dir, &["init", "-q"])?;
    git(dir, &["add", "-A"])?;
    git(dir, &["commit", "-q", "-m", "seed demo CLI"])?;
    git(dir, &["rev-parse", "HEAD"])
}

fn git(dir: &Path, args: &[&str]) -> std::io::Result<String> {
    // Identity and line endings pinned so the seed commit works on a machine
    // with no git identity and the file reads back byte-for-byte.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=wingman pilot",
            "-c",
            "user.email=pilot@wingman.local",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    Skipped,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// One provider's line in the matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixRow {
    pub provider: String,
    pub model: Option<String>,
    /// Static [`crate::provider_support`] tier, next to the live result.
    pub support: String,
    pub verdict: Verdict,
    pub detail: String,
    pub usd: f64,
    pub tokens: u64,
    pub wall_secs: f64,
}

impl MatrixRow {
    pub fn skipped(provider: &str, model: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model,
            support: crate::provider_support::classify(provider).to_string(),
            verdict: Verdict::Skipped,
            detail: reason.into(),
            usd: 0.0,
            tokens: 0,
            wall_secs: 0.0,
        }
    }
}

/// Seed a scratch repo at `root`, run the canned plan through the real
/// pipeline, and judge it. `inputs` gets the base commit and builds the
/// [`PipelineInputs`] (project root `root`, run id [`RUN_ID`]).
pub async fn run_canned_plan(
    root: &Path,
    provider: &str,
    model: &str,
    inputs: impl FnOnce(&str) -> PipelineInputs,
) -> MatrixRow {
    let started = Instant::now();
    let run_dir = crate::run_dir(root, RUN_ID);
    let result: Result<crate::pipeline::PipelineOutcome, String> = async {
        let base = init_scratch_repo(root).map_err(|e| format!("scratch repo: {e}"))?;
        let mut store = RunStore::create(
            &run_dir,
            RUN_ID,
            GOAL,
            &base,
            &crate::integration_branch(RUN_ID),
        )
        .await
        .map_err(|e| format!("run store: {e}"))?;
        crate::planner::persist_plan(&mut store, &canned_plan())
            .await
            .map_err(|e| format!("persisting plan: {e}"))?;
        crate::pipeline::run_to_completion(store, inputs(&base))
            .await
            .map_err(|e| format!("pipeline: {e}"))
    }
    .await;

    let state = RunStore::load(&run_dir)
        .await
        .ok()
        .map(|s| s.state().clone());
    let (verdict, detail) = match (&result, &state) {
        (Err(e), _) => (Verdict::Fail, e.clone()),
        (Ok(_), None) => (Verdict::Fail, "run state unreadable".into()),
        (Ok(outcome), Some(state)) => judge(root, outcome, state),
    };
    let totals = state.map(|s| s.totals).unwrap_or_default();
    MatrixRow {
        provider: provider.into(),
        model: Some(model.into()),
        support: crate::provider_support::classify(provider).to_string(),
        verdict,
        detail,
        usd: totals.usd,
        tokens: totals.tokens_in + totals.tokens_out,
        wall_secs: started.elapsed().as_secs_f64(),
    }
}

fn judge(
    root: &Path,
    outcome: &crate::pipeline::PipelineOutcome,
    state: &crate::model::RunState,
) -> (Verdict, String) {
    if !outcome.failed_tasks.is_empty() {
        let why = state
            .tasks
            .iter()
            .filter(|t| outcome.failed_tasks.contains(&t.id))
            .map(|t| match &t.outcome {
                Some(o) => format!("{} {:?}: {}", t.id, t.status, o.summary),
                None => format!("{} {:?}", t.id, t.status),
            })
            .collect::<Vec<_>>()
            .join("; ");
        return (Verdict::Fail, format!("task did not reach Done: {why}"));
    }
    if state.status != RunStatus::Done || state.tasks.iter().any(|t| t.status != TaskStatus::Done) {
        return (
            Verdict::Fail,
            format!("run ended {:?} without every task Done", state.status),
        );
    }
    // The worker saying it finished is not the check: the flag has to be on
    // the integration branch the run produced.
    let spec = format!("{}:{MAIN_RS}", state.integration_branch);
    match git(root, &["show", &spec]) {
        Ok(body) if body.contains(FLAG) => (Verdict::Pass, "flag merged".into()),
        Ok(_) => (
            Verdict::Fail,
            format!("run finished but {MAIN_RS} on the integration branch has no {FLAG}"),
        ),
        Err(e) => (Verdict::Fail, format!("reading integration branch: {e}")),
    }
}

/// The whole matrix, as written to `matrix.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixReport {
    pub generated_at: String,
    pub goal: String,
    pub max_usd: f64,
    pub max_total_tokens: u64,
    pub rows: Vec<MatrixRow>,
}

impl MatrixReport {
    pub fn any_failed(&self) -> bool {
        self.rows.iter().any(|r| r.verdict == Verdict::Fail)
    }

    pub fn any_ran(&self) -> bool {
        self.rows.iter().any(|r| r.verdict != Verdict::Skipped)
    }

    pub fn render_markdown(&self) -> String {
        use std::fmt::Write;
        let mut s = String::from("# Pilot provider validation\n\n");
        let _ = writeln!(
            s,
            "Generated {} by `wingman pilot validate-providers`. Canned goal: \
             \"{}\". Cap per provider: ${:.2} and {} tokens.\n",
            self.generated_at, self.goal, self.max_usd, self.max_total_tokens
        );
        s.push_str("| Provider | Model | Tier | Result | Spend | Tokens | Time | Detail |\n");
        s.push_str("| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for r in &self.rows {
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | ${:.4} | {} | {:.0}s | {} |",
                cell(&r.provider),
                cell(r.model.as_deref().unwrap_or("-")),
                cell(&r.support),
                r.verdict.as_str(),
                r.usd,
                r.tokens,
                r.wall_secs,
                cell(&r.detail),
            );
        }
        s
    }
}

/// A table cell: one line, no column breaks.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{
        fake_happy_spawner, OrchestratorConfig, SpawnContext, WorkerSpawner,
    };
    use crate::pipeline::tests::{AllOkCommandRunner, ScriptedProvider};
    use std::sync::Arc;

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    /// The fake worker, plus (when `edit`) the change a real worker would
    /// commit on its task branch.
    fn worker(edit: bool) -> WorkerSpawner {
        let happy = fake_happy_spawner();
        Arc::new(move |ctx: SpawnContext| {
            let happy = happy.clone();
            Box::pin(async move {
                if edit {
                    // Started first, as a real worker is, so the manager does
                    // not see the task still assignable while git runs.
                    let _ = ctx
                        .store
                        .lock()
                        .await
                        .append(crate::model::Event::TaskStatus {
                            t: RunStore::now(),
                            id: ctx.task.id.clone(),
                            status: TaskStatus::InProgress,
                            outcome: None,
                        })
                        .await;
                    let main = ctx.worktree.join(MAIN_RS);
                    let body = std::fs::read_to_string(&main).unwrap().replace(
                        "    println!(\"usage: demo [--version]\");",
                        "    if args.iter().any(|a| a == \"--version-only\") {
                                 println!(\"{VERSION}\");
        return;
    }
                             println!(\"usage: demo [--version] [--version-only]\");",
                    );
                    std::fs::write(&main, body).unwrap();
                    git(
                        &ctx.worktree,
                        &["commit", "-q", "-am", "add --version-only"],
                    )
                    .unwrap();
                }
                happy(ctx).await
            })
        })
    }

    fn inputs(root: &Path, base: &str, spawner: WorkerSpawner) -> PipelineInputs {
        PipelineInputs {
            provider: Arc::new(ScriptedProvider::new()),
            manager_model: "stub".into(),
            worker_spawner: spawner,
            base_branch: "main".into(),
            project_root: root.to_path_buf(),
            command_runner: Box::new(AllOkCommandRunner),
            no_pr: true,
            orchestrator_cfg: OrchestratorConfig {
                max_concurrent_agents: 1,
                task_timeout: std::time::Duration::from_secs(30),
                project_root: root.to_path_buf(),
                run_id: RUN_ID.into(),
                base_commit: base.into(),
                use_real_worktrees: true,
                max_usd: 0.5,
                max_total_tokens: 0,
                max_retries_per_task: 0,
                enforce_checkpoint_hygiene: false,
                desktop_inbox: None,
            },
            max_ticks: 16,
            tier: wingman_config::PilotTier::Copilot,
            worker_model: "stub".into(),
            stats_path: None,
            auto_approved: false,
            pr_config: wingman_config::PilotPrConfig::default(),
            security_config: wingman_config::PilotSecurityConfig::default(),
            disabled_tools: Vec::new(),
            run_reviewer: false,
            run_critic: false,
            reviewer_model: "stub".into(),
            sandbox_default_tier: "host".into(),
            sandbox_availability: crate::sandbox::TierAvailability {
                docker: false,
                vm: Err("test".into()),
            },
            dangerous_paths: Vec::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stub_provider_passes_when_the_flag_is_merged() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let row = run_canned_plan(&root, "anthropic", "stub", |base| {
            inputs(&root, base, worker(true))
        })
        .await;
        assert_eq!(row.verdict, Verdict::Pass, "{}", row.detail);
        assert_eq!(row.model.as_deref(), Some("stub"));
        assert_eq!(row.support, "native");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stub_provider_fails_when_nothing_lands() {
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        // The worker reports success without touching the file, the way a
        // model that mangles its tool calls can: the row must not pass.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let row = run_canned_plan(&root, "ollama", "stub", |base| {
            inputs(&root, base, worker(false))
        })
        .await;
        assert_eq!(row.verdict, Verdict::Fail);
        assert!(
            row.detail.contains("has no --version-only"),
            "{}",
            row.detail
        );
    }

    #[test]
    fn seed_does_not_already_satisfy_the_acceptance_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join(MAIN_RS), SEED_MAIN).unwrap();
        let results =
            crate::acceptance::run_acceptance_checks(&canned_plan()[0].acceptance, dir.path());
        assert!(!crate::acceptance::all_green(&results));
    }

    #[test]
    fn report_renders_one_row_per_provider_and_escapes_cells() {
        let report = MatrixReport {
            generated_at: "now".into(),
            goal: GOAL.into(),
            max_usd: 0.5,
            max_total_tokens: 1000,
            rows: vec![
                MatrixRow::skipped("openai", None, "no API key | set OPENAI_API_KEY"),
                MatrixRow {
                    verdict: Verdict::Pass,
                    model: Some("claude".into()),
                    usd: 0.0123,
                    tokens: 42,
                    ..MatrixRow::skipped("anthropic", None, "flag merged")
                },
            ],
        };
        let md = report.render_markdown();
        assert!(md.contains("| openai | - | openai-compat | skipped |"));
        assert!(md.contains("no API key \\| set OPENAI_API_KEY"));
        assert!(md.contains("| anthropic | claude | native | pass | $0.0123 | 42 |"));
        assert!(report.any_ran());
        assert!(!report.any_failed());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"verdict\":\"skipped\""));
        assert_eq!(serde_json::from_str::<MatrixReport>(&json).unwrap(), report);
    }
}
