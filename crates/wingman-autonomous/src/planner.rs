//! Planner: turn a user goal into a task DAG.
//!
//! Phase 2 implementation is a single LLM call. Phase 7.6 layers a
//! repo-grounding + critique + rewrite loop on top (E2); the structure of
//! `plan_from_goal` is shaped so that grounding facts can be injected as an
//! extra message without changing the surface.
//!
//! ## Test seam
//!
//! The provider call is funneled through [`PlannerLlm`]. Production wires
//! it up against [`wingman_core::Provider`]; tests pass a closure that
//! returns canned JSON, so the planner is exercised end-to-end without
//! hitting the network.

use std::collections::HashSet;
use std::path::Path;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use wingman_core::{
    CacheBreakpoint, CompletionRequest, Message, Provider, StopReason, StreamEvent,
};

use crate::model::{Acceptance, Event, Reversibility, Role, Task, TaskStatus};
use crate::role::load_planner_prompt;
use crate::store::RunStore;

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("model returned no text")]
    EmptyResponse,
    #[error("model output was not valid JSON: {0}")]
    BadJson(serde_json::Error),
    #[error("plan rejected: {0}")]
    BadPlan(String),
    #[error("provider error: {0}")]
    Provider(#[from] wingman_core::WingmanError),
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
}

/// What we expect the planner LLM to emit. Mirrors the JSON described in
/// `prompts/manager-planner.md` — the shape is intentionally a subset of
/// our internal [`Task`] so users can hand-write a plan file too.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTask {
    pub id: String,
    pub role: Role,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<Acceptance>,
    #[serde(default)]
    pub reversibility: Reversibility,
    #[serde(default)]
    pub reversibility_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerOutput {
    pub tasks: Vec<PlannedTask>,
}

/// Abstraction over "send these messages to a model, get text back". Lets
/// tests substitute a canned reply for the real provider.
#[async_trait::async_trait]
pub trait PlannerLlm: Send + Sync {
    async fn complete(
        &self,
        system: String,
        user: String,
    ) -> Result<String, wingman_core::WingmanError>;
}

/// Default implementation that talks to an `wingman_core::Provider`. Uses
/// no tools — the planner is a pure structured-text call.
pub struct ProviderLlm<'p> {
    pub provider: &'p dyn Provider,
    pub model: String,
    pub max_tokens: u32,
}

#[async_trait::async_trait]
impl<'p> PlannerLlm for ProviderLlm<'p> {
    async fn complete(
        &self,
        system: String,
        user: String,
    ) -> Result<String, wingman_core::WingmanError> {
        let req = CompletionRequest {
            model: self.model.clone(),
            system: Some(system),
            messages: vec![Message::user_text(user)],
            tools: Vec::new(),
            max_tokens: self.max_tokens,
            temperature: Some(0.2),
            // The planner's draft→rewrite passes (and the refiner) share this
            // system prompt, so caching it lets the second call read it back
            // instead of re-billing the full prompt.
            cache_breakpoints: vec![CacheBreakpoint::AfterSystem],

            // These are structured-output helper calls (review verdict, task plan),
            // not the agent loop. Reasoning would add cost and latency for output
            // that has to parse as JSON either way.
            reasoning: wingman_core::ReasoningEffort::Off,
        };
        let mut stream = self.provider.complete(req).await?;
        let mut out = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                StreamEvent::TextDelta { text } => out.push_str(&text),
                StreamEvent::Stop { reason } => {
                    if matches!(reason, StopReason::EndTurn | StopReason::Other) {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Parse the LLM's raw output into validated [`PlannedTask`]s.
///
/// Tolerates leading/trailing Markdown fences and prose — extracts the
/// first balanced JSON object. Models drift; the planner prompt is firm
/// but we don't fail the whole run over a stray code fence.
pub fn parse_plan(raw: &str) -> Result<Vec<PlannedTask>, PlannerError> {
    let json_str = extract_json_object(raw).unwrap_or_else(|| raw.to_string());
    let parsed: PlannerOutput = serde_json::from_str(&json_str).map_err(PlannerError::BadJson)?;
    validate_plan(&parsed.tasks)?;
    Ok(parsed.tasks)
}

/// Validate a plan: non-empty, unique ids, deps reference known ids, no
/// cycles, no two tasks with overlapping `writes` and no dep edge between
/// them (catches the write-set conflict that E4's scheduler would otherwise
/// hit at run time).
pub fn validate_plan(tasks: &[PlannedTask]) -> Result<(), PlannerError> {
    if tasks.is_empty() {
        return Err(PlannerError::BadPlan("planner produced zero tasks".into()));
    }
    let mut seen = HashSet::new();
    for t in tasks {
        if t.id.is_empty() {
            return Err(PlannerError::BadPlan("task with empty id".into()));
        }
        if !seen.insert(t.id.clone()) {
            return Err(PlannerError::BadPlan(format!(
                "duplicate task id: {}",
                t.id
            )));
        }
        if t.title.trim().is_empty() {
            return Err(PlannerError::BadPlan(format!(
                "task {} has empty title",
                t.id
            )));
        }
    }
    for t in tasks {
        for d in &t.deps {
            if !seen.contains(d) {
                return Err(PlannerError::BadPlan(format!(
                    "task {} depends on unknown id {}",
                    t.id, d
                )));
            }
            if d == &t.id {
                return Err(PlannerError::BadPlan(format!(
                    "task {} depends on itself",
                    t.id
                )));
            }
        }
    }
    if has_cycle(tasks) {
        return Err(PlannerError::BadPlan("plan has a dependency cycle".into()));
    }
    Ok(())
}

fn has_cycle(tasks: &[PlannedTask]) -> bool {
    // Delegate to the shared checker so the planner's static validation and
    // the orchestrator's runtime mutation guards use one implementation.
    let edges: std::collections::HashMap<String, Vec<String>> = tasks
        .iter()
        .map(|t| (t.id.clone(), t.deps.clone()))
        .collect();
    crate::scheduler::edges_have_cycle(&edges)
}

/// Convert a planned task into an internal [`Task`] (status = `Pending`).
pub fn task_from_planned(planned: &PlannedTask) -> Task {
    Task {
        id: planned.id.clone(),
        role: planned.role.clone(),
        title: planned.title.clone(),
        goal: planned.goal.clone(),
        deps: planned.deps.clone(),
        writes: planned.writes.clone(),
        acceptance: planned.acceptance.clone(),
        reversibility: planned.reversibility,
        reversibility_reason: planned.reversibility_reason.clone(),
        status: TaskStatus::Pending,
        agent: None,
        worktree: None,
        usd: 0.0,
        commits: Vec::new(),
        outcome: None,
        started_at: None,
        ended_at: None,
        attempts: 0,
    }
}

/// Convert a planned task into its `task.create` event.
pub fn create_event_from_planned(planned: &PlannedTask) -> Event {
    Event::TaskCreate {
        t: RunStore::now(),
        id: planned.id.clone(),
        role: planned.role.clone(),
        title: planned.title.clone(),
        goal: planned.goal.clone(),
        deps: planned.deps.clone(),
        writes: planned.writes.clone(),
        acceptance: planned.acceptance.clone(),
        reversibility: planned.reversibility,
        reversibility_reason: planned.reversibility_reason.clone(),
    }
}

/// Render a plan for human review (the y/e/n prompt).
pub fn render_plan(tasks: &[PlannedTask]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for (i, t) in tasks.iter().enumerate() {
        let deps = if t.deps.is_empty() {
            "—".to_string()
        } else {
            t.deps.join(", ")
        };
        writeln!(
            &mut out,
            "  {n}. [{role}] {title} (deps: {deps})",
            n = i + 1,
            role = t.role.as_str(),
            title = t.title,
        )
        .ok();
        if !t.goal.trim().is_empty() {
            for line in t.goal.lines() {
                writeln!(&mut out, "        {line}").ok();
            }
        }
    }
    out
}

/// Single-pass plan call. Kept as a low-level building block for tests
/// and for callers that explicitly want to skip the grounding +
/// critique + rewrite passes. Most callers should use
/// [`plan_from_goal`] instead, which wraps this with E2's multi-pass
/// machinery.
pub async fn plan_from_goal_oneshot(
    llm: &dyn PlannerLlm,
    goal: &str,
) -> Result<Vec<PlannedTask>, PlannerError> {
    let system = load_planner_prompt();
    let user = format!(
        "GOAL:\n{goal}\n\nProduce the task DAG as JSON now. \
         Respond with ONLY the JSON object — no prose, no Markdown fences."
    );
    let raw = llm.complete(system, user).await?;
    if raw.trim().is_empty() {
        return Err(PlannerError::EmptyResponse);
    }
    parse_plan(&raw)
}

/// Two-pass repo-aware plan call (E2).
///
/// 1. Grounding pass: scan the repo for keyword matches against the
///    goal, build a facts block ([`crate::grounding`]).
/// 2. Draft pass: ask the LLM to produce a plan conditioned on the
///    facts.
/// 3. Static critique: structural validation (paths exist, no
///    overlapping writes in the same dep layer) via [`critique_plan`].
/// 4. Rewrite pass: if the critique surfaced issues, ask the LLM to
///    produce a revised plan that fixes them. Re-validate; fall back to
///    the original plan if the rewrite is worse.
///
/// `repo_root` is the project root used for grounding + critique;
/// passing an empty path disables grounding (useful for tests).
pub async fn plan_from_goal(
    llm: &dyn PlannerLlm,
    goal: &str,
    repo_root: &Path,
) -> Result<Vec<PlannedTask>, PlannerError> {
    plan_from_goal_with_priming(llm, goal, repo_root, None).await
}

/// Like [`plan_from_goal`], but injects an optional E6 priming block
/// (rendered from past similar runs by [`crate::learning::render_priming`])
/// ahead of the grounding facts so the draft pass can condition on what
/// worked or got reverted before. `priming = None` is identical to
/// [`plan_from_goal`].
pub async fn plan_from_goal_with_priming(
    llm: &dyn PlannerLlm,
    goal: &str,
    repo_root: &Path,
    priming: Option<&str>,
) -> Result<Vec<PlannedTask>, PlannerError> {
    let facts = if repo_root.as_os_str().is_empty() {
        None
    } else {
        let keywords = crate::grounding::extract_keywords(goal);
        let block = crate::grounding::scan_repo_for_facts(repo_root, &keywords);
        Some(crate::grounding::render_facts(&block))
    };

    let priming_block = priming
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("\n{p}\n"))
        .unwrap_or_default();
    let system = load_planner_prompt();
    let user = format!(
        "GOAL:\n{goal}\n{priming_block}{facts_block}\nProduce the task DAG as JSON now. \
         Respond with ONLY the JSON object — no prose, no Markdown fences.",
        facts_block = facts
            .as_deref()
            .map(|f| format!("\n{f}\n"))
            .unwrap_or_default(),
    );
    let raw = llm.complete(system.clone(), user.clone()).await?;
    if raw.trim().is_empty() {
        return Err(PlannerError::EmptyResponse);
    }
    let draft = parse_plan(&raw)?;

    // Static critique: catches issues we can detect without another LLM
    // round-trip. If everything passes, ship the draft.
    let report = critique_plan(&draft, repo_root);
    if report.is_clean() {
        return Ok(draft);
    }

    // Rewrite pass: feed the model its own plan + the critique and ask
    // for a fix. If the rewrite is unparseable or *worse*, fall back to
    // the draft — the planner shouldn't trade a flawed plan for a
    // hallucinated rewrite.
    let critique_block = render_critique_for_rewrite(&report);
    let rewrite_user = format!(
        "GOAL:\n{goal}\n{facts_block}\n\
         Your previous plan was:\n```json\n{prev}\n```\n\n\
         The static critique found these issues:\n{critique_block}\n\n\
         Produce a revised JSON plan that fixes every issue. Same shape \
         as before; respond with ONLY the JSON object.",
        facts_block = facts
            .as_deref()
            .map(|f| format!("\n{f}\n"))
            .unwrap_or_default(),
        prev = serde_json::to_string_pretty(&PlannerOutput {
            tasks: draft.clone()
        })
        .unwrap_or_else(|_| "{}".to_string()),
    );
    match llm.complete(system, rewrite_user).await {
        Ok(rewritten_raw) if !rewritten_raw.trim().is_empty() => match parse_plan(&rewritten_raw) {
            Ok(revised) => {
                let revised_report = critique_plan(&revised, repo_root);
                if revised_report.is_clean() || revised_report.score() < report.score() {
                    Ok(revised)
                } else {
                    tracing::warn!(
                        target: "pilot::planner",
                        "rewrite pass did not improve the plan; using draft"
                    );
                    Ok(draft)
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "pilot::planner",
                    error = %e,
                    "rewrite pass produced an unparseable plan; using draft"
                );
                Ok(draft)
            }
        },
        _ => Ok(draft),
    }
}

/// Output of [`critique_plan`]. Empty fields = clean plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CritiqueReport {
    /// Task writes whose containing directory does not exist in the
    /// repo. Likely hallucinated paths.
    pub hallucinated_paths: Vec<(String, String)>,
    /// Pairs of (task_a, task_b) where the two tasks could run
    /// concurrently (no dep edge between them) AND their writes
    /// overlap on at least one path. E4's scheduler would either
    /// linearise them or hit a conflict.
    pub overlapping_writes: Vec<(String, String, String)>,
    /// Tasks whose `acceptance` is empty — failures from those tasks
    /// will be undetectable. (E3 still ships, but the planner can do
    /// better than empty acceptance.)
    pub missing_acceptance: Vec<String>,
    /// `(task, pattern, path)` for a `grep` acceptance check that can never
    /// pass: its path neither exists in the repo nor appears in any task's
    /// `writes`, so no amount of correct work will satisfy it. The worker
    /// burns its whole retry ladder discovering this.
    pub unpassable_acceptance: Vec<(String, String, String)>,
    /// `(task, pattern, path)` for a `grep` acceptance check whose only
    /// evidence is a file the same task creates from scratch. It cannot fail
    /// for any reason except the worker forgetting to type the pattern, so it
    /// verifies nothing about the behaviour that was actually asked for.
    ///
    /// Worth flagging because it is how a plan built on an imagined layout
    /// hides: inventing `args.rs` and then greping `args.rs` looks like a
    /// verified task right up until a human reads the diff.
    pub vacuous_acceptance: Vec<(String, String, String)>,
}

impl CritiqueReport {
    pub fn is_clean(&self) -> bool {
        self.hallucinated_paths.is_empty()
            && self.overlapping_writes.is_empty()
            && self.missing_acceptance.is_empty()
            && self.unpassable_acceptance.is_empty()
            && self.vacuous_acceptance.is_empty()
    }
    /// Severity score — lower is better. Used to decide whether the
    /// rewrite pass actually improved things.
    pub fn score(&self) -> usize {
        // An unpassable check is the most expensive defect here: it does not
        // degrade the run, it guarantees the task fails after spending a full
        // retry ladder. Weighted above a hallucinated path accordingly.
        self.unpassable_acceptance.len() * 4
            + self.vacuous_acceptance.len() * 3
            + self.hallucinated_paths.len() * 3
            + self.overlapping_writes.len() * 2
            + self.missing_acceptance.len()
    }
}

/// Static plan critique. Pure function — runs no I/O beyond stat'ing
/// directories that the plan references.
pub fn critique_plan(plan: &[PlannedTask], repo_root: &Path) -> CritiqueReport {
    let mut report = CritiqueReport::default();
    let check_paths = !repo_root.as_os_str().is_empty();
    // Any path the plan says it will create counts as reachable, whichever
    // task creates it — greping a file your dependency writes is fine.
    let all_writes: HashSet<&str> = plan
        .iter()
        .flat_map(|t| t.writes.iter().map(String::as_str))
        .collect();

    for t in plan {
        // Hallucinated paths: flag writes whose parent directory does
        // not exist. The planner is allowed to create NEW files but
        // not new top-level crates by accident.
        if check_paths {
            for w in &t.writes {
                if w.contains('*') || w.contains('?') {
                    continue; // skip globs — they're patterns, not paths
                }
                let path = Path::new(w);
                if let Some(parent) = path.parent() {
                    if parent.as_os_str().is_empty() {
                        continue;
                    }
                    let abs = repo_root.join(parent);
                    if !abs.exists() {
                        report.hallucinated_paths.push((t.id.clone(), w.clone()));
                    }
                }
            }
        }
        if t.acceptance.is_empty() && !matches!(t.role, crate::model::Role::Reviewer) {
            report.missing_acceptance.push(t.id.clone());
        }
        // A grep acceptance against a path that neither exists nor is going
        // to be written cannot pass, however well the worker performs. Left
        // unchecked it reads as a task failure, so the retry ladder runs to
        // exhaustion re-attempting work that was never the problem.
        if check_paths {
            for a in &t.acceptance {
                let Acceptance::Grep { path, pattern } = a else {
                    continue;
                };
                if repo_root.join(path).exists() {
                    continue;
                }
                // The file does not exist yet. Who is going to create it?
                if t.writes.iter().any(|w| w == path) {
                    // This very task. Then the check proves only that the
                    // worker wrote the string it was told to write.
                    report
                        .vacuous_acceptance
                        .push((t.id.clone(), pattern.clone(), path.clone()));
                } else if !all_writes.contains(path.as_str()) {
                    // Nobody. Unsatisfiable however well the worker performs.
                    report.unpassable_acceptance.push((
                        t.id.clone(),
                        pattern.clone(),
                        path.clone(),
                    ));
                }
            }
        }
    }

    // Overlapping writes between tasks that COULD run concurrently —
    // i.e. neither is a transitive dep of the other.
    let by_id: std::collections::HashMap<&str, &PlannedTask> =
        plan.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut pairs_seen: HashSet<(String, String)> = HashSet::new();
    for a in plan {
        for b in plan {
            if a.id >= b.id {
                continue;
            }
            let key = (a.id.clone(), b.id.clone());
            if !pairs_seen.insert(key.clone()) {
                continue;
            }
            if reachable_via_deps(a.id.as_str(), b.id.as_str(), &by_id)
                || reachable_via_deps(b.id.as_str(), a.id.as_str(), &by_id)
            {
                continue; // ordered, can't overlap
            }
            // Find overlapping paths.
            for w in &a.writes {
                if b.writes.contains(w) {
                    report
                        .overlapping_writes
                        .push((a.id.clone(), b.id.clone(), w.clone()));
                }
            }
        }
    }
    report
}

/// Is `target` reachable from `start` by following `deps` edges?
fn reachable_via_deps(
    start: &str,
    target: &str,
    by_id: &std::collections::HashMap<&str, &PlannedTask>,
) -> bool {
    let mut stack: Vec<&str> = vec![start];
    let mut visited: HashSet<String> = HashSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node.to_string()) {
            continue;
        }
        let Some(t) = by_id.get(node) else { continue };
        for d in &t.deps {
            if d == target {
                return true;
            }
            stack.push(d.as_str());
        }
    }
    false
}

/// Render a [`CritiqueReport`] as a bullet list the LLM can read in
/// the rewrite pass.
fn render_critique_for_rewrite(report: &CritiqueReport) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for (id, path) in &report.hallucinated_paths {
        let _ = writeln!(
            s,
            "- task {id} writes `{path}` — the containing directory does not exist in the repo; either fix the path or split into a new task that creates the directory first"
        );
    }
    for (a, b, path) in &report.overlapping_writes {
        let _ = writeln!(
            s,
            "- tasks {a} and {b} both write `{path}` but have no dep edge between them — add `deps: [{a}]` to {b} (or vice versa), or split them"
        );
    }
    for id in &report.missing_acceptance {
        let _ = writeln!(
            s,
            "- task {id} has no `acceptance` checks; add at least one executable shell or grep check so the orchestrator can verify completion"
        );
    }
    for (id, pattern, path) in &report.vacuous_acceptance {
        let _ = writeln!(
            s,
            "- task {id} greps `{pattern}` in `{path}`, a file the same task creates from scratch — that only proves the worker typed the pattern, and it hides the case where `{path}` is not where this code belongs. Check the facts block for the file that already holds this behaviour and grep that instead; if the new file is genuinely right, verify the behaviour with a shell check as well"
        );
    }
    for (id, pattern, path) in &report.unpassable_acceptance {
        let _ = writeln!(
            s,
            "- task {id} greps `{pattern}` in `{path}`, but that file does not exist and no task declares it in `writes` — this check can never pass. Point it at the file that really holds this code (see the facts block), or add `{path}` to that task's `writes` if the task is genuinely meant to create it"
        );
    }
    s
}

/// Persist a plan into a run store as a batch of `task.create` events.
pub async fn persist_plan(store: &mut RunStore, plan: &[PlannedTask]) -> Result<(), PlannerError> {
    for t in plan {
        store.append(create_event_from_planned(t)).await?;
    }
    Ok(())
}

/// Extract the first balanced top-level JSON object from a string. Used to
/// peel off Markdown fences or surrounding prose when models stray from
/// the "respond with only JSON" instruction.
fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CannedLlm(String);

    #[async_trait::async_trait]
    impl PlannerLlm for CannedLlm {
        async fn complete(
            &self,
            _system: String,
            _user: String,
        ) -> Result<String, wingman_core::WingmanError> {
            Ok(self.0.clone())
        }
    }

    fn sample_plan_json() -> &'static str {
        r#"{
            "tasks": [
                {
                    "id": "t1",
                    "role": "developer",
                    "title": "Add --version-only flag",
                    "goal": "Wire fast-exit flag",
                    "deps": [],
                    "writes": ["crates/wingman-cli/src/args.rs"],
                    "acceptance": [
                        {"kind": "shell", "cmd": "cargo check -p wingman-cli"}
                    ],
                    "reversibility": "trivial"
                },
                {
                    "id": "t2",
                    "role": "tester",
                    "title": "Smoke test for --version-only",
                    "deps": ["t1"],
                    "writes": ["crates/wingman-cli/tests/version.rs"],
                    "acceptance": [
                        {"kind": "shell", "cmd": "cargo test -p wingman-cli version"}
                    ]
                },
                {
                    "id": "t3",
                    "role": "reviewer",
                    "title": "Approve diff",
                    "deps": ["t1", "t2"]
                }
            ]
        }"#
    }

    #[tokio::test]
    async fn happy_path_parses_and_validates() {
        let llm = CannedLlm(sample_plan_json().to_string());
        let plan = plan_from_goal(&llm, "add --version-only", Path::new(""))
            .await
            .unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].id, "t1");
        assert_eq!(plan[2].deps, vec!["t1", "t2"]);
    }

    #[test]
    fn extract_json_strips_markdown_fence() {
        let s = "Here is the plan:\n```json\n{\"tasks\":[]}\n```\nthanks!";
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, "{\"tasks\":[]}");
    }

    #[test]
    fn extract_json_handles_strings_with_braces() {
        let s = r#"{"tasks":[{"id":"t1","title":"x { y } z"}]}"#;
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, s);
    }

    #[test]
    fn validate_rejects_empty() {
        let err = validate_plan(&[]).unwrap_err();
        assert!(matches!(err, PlannerError::BadPlan(_)));
    }

    #[test]
    fn validate_rejects_duplicate_id() {
        let plan = vec![
            PlannedTask {
                id: "t1".into(),
                role: Role::Developer,
                title: "x".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec![],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
            PlannedTask {
                id: "t1".into(),
                role: Role::Tester,
                title: "y".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec![],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
        ];
        let err = validate_plan(&plan).unwrap_err();
        assert!(matches!(err, PlannerError::BadPlan(_)));
    }

    #[test]
    fn validate_rejects_unknown_dep() {
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "x".into(),
            goal: "".into(),
            deps: vec!["t0".into()],
            writes: vec![],
            acceptance: vec![],
            reversibility: Default::default(),
            reversibility_reason: None,
        }];
        let err = validate_plan(&plan).unwrap_err();
        assert!(matches!(err, PlannerError::BadPlan(_)));
    }

    #[test]
    fn validate_rejects_cycle() {
        let plan = vec![
            PlannedTask {
                id: "t1".into(),
                role: Role::Developer,
                title: "x".into(),
                goal: "".into(),
                deps: vec!["t2".into()],
                writes: vec![],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
            PlannedTask {
                id: "t2".into(),
                role: Role::Developer,
                title: "y".into(),
                goal: "".into(),
                deps: vec!["t1".into()],
                writes: vec![],
                acceptance: vec![],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
        ];
        let err = validate_plan(&plan).unwrap_err();
        assert!(matches!(err, PlannerError::BadPlan(_)));
    }

    #[tokio::test]
    async fn persist_plan_writes_task_create_events() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let mut store = RunStore::create(dir.path(), "r1", "g", "abc", "wingman/auto/r1")
            .await
            .unwrap();
        let plan = parse_plan(sample_plan_json()).unwrap();
        persist_plan(&mut store, &plan).await.unwrap();
        assert_eq!(store.state().tasks.len(), 3);
        assert_eq!(store.state().tasks[0].id, "t1");
    }

    #[test]
    fn critique_flags_hallucinated_paths() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("crates")).unwrap();
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "x".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec!["crates/nonexistent/src/main.rs".into()],
            acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
            reversibility: Default::default(),
            reversibility_reason: None,
        }];
        let report = critique_plan(&plan, dir.path());
        assert_eq!(report.hallucinated_paths.len(), 1);
        assert_eq!(report.hallucinated_paths[0].0, "t1");
        assert!(report.hallucinated_paths[0].1.contains("nonexistent"));
    }

    #[test]
    fn critique_allows_new_files_in_existing_dirs() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/wingman-cli/src")).unwrap();
        let plan = vec![PlannedTask {
            id: "t1".into(),
            role: Role::Developer,
            title: "x".into(),
            goal: "".into(),
            deps: vec![],
            writes: vec!["crates/wingman-cli/src/brand_new.rs".into()],
            acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
            reversibility: Default::default(),
            reversibility_reason: None,
        }];
        let report = critique_plan(&plan, dir.path());
        assert!(
            report.hallucinated_paths.is_empty(),
            "creating a new file in an existing dir should not flag"
        );
    }

    #[test]
    fn critique_detects_overlapping_writes_without_dep_edge() {
        // No tempdir needed — we disable path-existence by passing
        // empty root, so we're only exercising the overlap detection.
        let plan = vec![
            PlannedTask {
                id: "t1".into(),
                role: Role::Developer,
                title: "a".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec!["src/main.rs".into()],
                acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
            PlannedTask {
                id: "t2".into(),
                role: Role::Developer,
                title: "b".into(),
                goal: "".into(),
                deps: vec![], // NO dep edge to t1
                writes: vec!["src/main.rs".into()],
                acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
        ];
        // Disable path-existence check by passing empty root.
        let report = critique_plan(&plan, Path::new(""));
        assert_eq!(report.overlapping_writes.len(), 1);
        let (a, b, p) = &report.overlapping_writes[0];
        assert!((a == "t1" && b == "t2") || (a == "t2" && b == "t1"));
        assert_eq!(p, "src/main.rs");
    }

    #[test]
    fn critique_allows_overlapping_writes_when_dep_edge_orders_them() {
        let plan = vec![
            PlannedTask {
                id: "t1".into(),
                role: Role::Developer,
                title: "a".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec!["src/main.rs".into()],
                acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
            PlannedTask {
                id: "t2".into(),
                role: Role::Developer,
                title: "b".into(),
                goal: "".into(),
                deps: vec!["t1".into()],
                writes: vec!["src/main.rs".into()],
                acceptance: vec![Acceptance::Shell { cmd: "true".into() }],
                reversibility: Default::default(),
                reversibility_reason: None,
            },
        ];
        let report = critique_plan(&plan, Path::new(""));
        assert!(report.overlapping_writes.is_empty());
    }

    #[test]
    fn critique_flags_missing_acceptance_except_reviewers() {
        let plan = vec![
            PlannedTask {
                id: "t1".into(),
                role: Role::Developer,
                title: "a".into(),
                goal: "".into(),
                deps: vec![],
                writes: vec!["src/x.rs".into()],
                acceptance: vec![], // missing
                reversibility: Default::default(),
                reversibility_reason: None,
            },
            PlannedTask {
                id: "t2".into(),
                role: Role::Reviewer,
                title: "b".into(),
                goal: "".into(),
                deps: vec!["t1".into()],
                writes: vec![],
                acceptance: vec![], // reviewer with empty acceptance is fine
                reversibility: Default::default(),
                reversibility_reason: None,
            },
        ];
        let report = critique_plan(&plan, Path::new(""));
        assert_eq!(report.missing_acceptance, vec!["t1"]);
    }

    #[test]
    fn two_pass_uses_rewrite_when_static_critique_finds_issues() {
        // CannedLlm returns the SAME plan on both calls; the rewrite
        // path runs but doesn't fix anything, so we fall back to the
        // draft. Verifies the flow doesn't error out.
        let llm = CannedLlm(sample_plan_json().to_string());
        let plan =
            futures::executor::block_on(plan_from_goal(&llm, "add --version-only", Path::new("")))
                .unwrap();
        assert_eq!(plan.len(), 3);
    }

    #[test]
    fn render_plan_is_non_empty() {
        let plan = parse_plan(sample_plan_json()).unwrap();
        let s = render_plan(&plan);
        assert!(s.contains("--version-only"));
        assert!(s.contains("[developer]"));
    }

    /// A `PlannedTask` with everything not under test left empty.
    fn pt(id: &str, role: Role, title: &str) -> PlannedTask {
        PlannedTask {
            id: id.into(),
            role,
            title: title.into(),
            goal: String::new(),
            deps: vec![],
            writes: vec![],
            acceptance: vec![],
            reversibility: Reversibility::default(),
            reversibility_reason: None,
        }
    }

    /// The exact plan sonnet-5 produced for "add a --version-only flag to
    /// wingman-cli" on 2026-08-21: it invented `args.rs` (this CLI parses
    /// args in `cli.rs`) and hard-coded that invention into a grep check.
    /// `args.rs` was in `writes`, so the hallucinated-path check could not
    /// see it -- the containing directory is real. The run spent $1.15 and a
    /// full retry ladder before deadlocking.
    #[test]
    fn unpassable_grep_is_caught_when_the_file_is_created_by_the_task() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/wingman-cli/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("cli.rs"), "pub struct Cli {}").unwrap();

        let mut t = pt("t1", Role::Developer, "Add --version-only flag");
        t.writes = vec![
            "crates/wingman-cli/src/args.rs".into(),
            "crates/wingman-cli/src/main.rs".into(),
        ];
        t.acceptance = vec![Acceptance::Grep {
            pattern: "version_only".into(),
            path: "crates/wingman-cli/src/args.rs".into(),
        }];

        let report = critique_plan(std::slice::from_ref(&t), dir.path());
        assert!(
            report.hallucinated_paths.is_empty(),
            "the directory is real, so the existing check stays quiet"
        );
        assert_eq!(
            report.vacuous_acceptance,
            vec![(
                "t1".to_string(),
                "version_only".to_string(),
                "crates/wingman-cli/src/args.rs".to_string()
            )],
            "the task greps a file it creates itself -- proves nothing"
        );
        assert!(!report.is_clean(), "must trigger the rewrite pass");
        assert!(render_critique_for_rewrite(&report).contains("creates from scratch"));

        // Pointed at the file that really holds the code, it is clean.
        t.acceptance = vec![Acceptance::Grep {
            pattern: "version_only".into(),
            path: "crates/wingman-cli/src/cli.rs".into(),
        }];
        let report = critique_plan(std::slice::from_ref(&t), dir.path());
        assert!(report.vacuous_acceptance.is_empty());
        assert!(report.unpassable_acceptance.is_empty());
    }

    #[test]
    fn grep_against_a_file_a_dependency_creates_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        let mut t1 = pt("t1", Role::Developer, "create the module");
        t1.writes = vec!["src/new_module.rs".into()];
        t1.acceptance = vec![Acceptance::Shell {
            cmd: "cargo check".into(),
        }];
        let mut t2 = pt("t2", Role::Tester, "test it");
        t2.deps = vec!["t1".into()];
        t2.acceptance = vec![Acceptance::Grep {
            pattern: "fn new_thing".into(),
            path: "src/new_module.rs".into(),
        }];
        let plan = vec![t1, t2];
        let report = critique_plan(&plan, dir.path());
        assert!(
            report.unpassable_acceptance.is_empty(),
            "a file a dependency writes is reachable, not unpassable"
        );
    }

    #[test]
    fn path_checks_are_skipped_without_a_repo_root() {
        let mut t = pt("t1", Role::Developer, "x");
        t.acceptance = vec![Acceptance::Grep {
            pattern: "p".into(),
            path: "nowhere/at/all.rs".into(),
        }];
        let plan = vec![t];
        let report = critique_plan(&plan, Path::new(""));
        assert!(report.unpassable_acceptance.is_empty());
    }
}
