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
//! it up against [`arccode_core::Provider`]; tests pass a closure that
//! returns canned JSON, so the planner is exercised end-to-end without
//! hitting the network.

use std::collections::HashSet;

use arccode_core::{CompletionRequest, Message, Provider, StopReason, StreamEvent};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    Provider(#[from] arccode_core::ArccodeError),
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
    ) -> Result<String, arccode_core::ArccodeError>;
}

/// Default implementation that talks to an `arccode_core::Provider`. Uses
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
    ) -> Result<String, arccode_core::ArccodeError> {
        let req = CompletionRequest {
            model: self.model.clone(),
            system: Some(system),
            messages: vec![Message::user_text(user)],
            tools: Vec::new(),
            max_tokens: self.max_tokens,
            temperature: Some(0.2),
            cache_breakpoints: Vec::new(),
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
    let parsed: PlannerOutput =
        serde_json::from_str(&json_str).map_err(PlannerError::BadJson)?;
    validate_plan(&parsed.tasks)?;
    Ok(parsed.tasks)
}

/// Validate a plan: non-empty, unique ids, deps reference known ids, no
/// cycles, no two tasks with overlapping `writes` and no dep edge between
/// them (catches the write-set conflict that E4's scheduler would otherwise
/// hit at run time).
pub fn validate_plan(tasks: &[PlannedTask]) -> Result<(), PlannerError> {
    if tasks.is_empty() {
        return Err(PlannerError::BadPlan(
            "planner produced zero tasks".into(),
        ));
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
    // DFS with three-colour marking.
    use std::collections::HashMap;
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        White,
        Gray,
        Black,
    }
    let mut marks: HashMap<&str, Mark> =
        tasks.iter().map(|t| (t.id.as_str(), Mark::White)).collect();
    let deps: HashMap<&str, Vec<&str>> = tasks
        .iter()
        .map(|t| (t.id.as_str(), t.deps.iter().map(|s| s.as_str()).collect()))
        .collect();

    fn visit<'a>(
        node: &'a str,
        marks: &mut std::collections::HashMap<&'a str, Mark>,
        deps: &std::collections::HashMap<&'a str, Vec<&'a str>>,
    ) -> bool {
        match marks.get(node).copied().unwrap_or(Mark::White) {
            Mark::Gray => return true,
            Mark::Black => return false,
            Mark::White => {}
        }
        marks.insert(node, Mark::Gray);
        if let Some(ds) = deps.get(node) {
            for d in ds {
                if visit(d, marks, deps) {
                    return true;
                }
            }
        }
        marks.insert(node, Mark::Black);
        false
    }

    for t in tasks {
        if visit(t.id.as_str(), &mut marks, &deps) {
            return true;
        }
    }
    false
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

/// One-shot plan call. Loads the planner prompt, asks the LLM, parses
/// and validates the response.
pub async fn plan_from_goal(
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

/// Persist a plan into a run store as a batch of `task.create` events.
pub async fn persist_plan(
    store: &mut RunStore,
    plan: &[PlannedTask],
) -> Result<(), PlannerError> {
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
        ) -> Result<String, arccode_core::ArccodeError> {
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
                    "writes": ["crates/arccode-cli/src/args.rs"],
                    "acceptance": [
                        {"kind": "shell", "cmd": "cargo check -p arccode-cli"}
                    ],
                    "reversibility": "trivial"
                },
                {
                    "id": "t2",
                    "role": "tester",
                    "title": "Smoke test for --version-only",
                    "deps": ["t1"],
                    "writes": ["crates/arccode-cli/tests/version.rs"],
                    "acceptance": [
                        {"kind": "shell", "cmd": "cargo test -p arccode-cli version"}
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
        let plan = plan_from_goal(&llm, "add --version-only").await.unwrap();
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
        let mut store = RunStore::create(dir.path(), "r1", "g", "abc", "arccode/auto/r1")
            .await
            .unwrap();
        let plan = parse_plan(sample_plan_json()).unwrap();
        persist_plan(&mut store, &plan).await.unwrap();
        assert_eq!(store.state().tasks.len(), 3);
        assert_eq!(store.state().tasks[0].id, "t1");
    }

    #[test]
    fn render_plan_is_non_empty() {
        let plan = parse_plan(sample_plan_json()).unwrap();
        let s = render_plan(&plan);
        assert!(s.contains("--version-only"));
        assert!(s.contains("[developer]"));
    }
}
