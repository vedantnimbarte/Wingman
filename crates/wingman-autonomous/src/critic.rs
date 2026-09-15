//! J10 — critic agent (always-on red team).
//!
//! A second model — ideally a *different family* than the primary, so its
//! errors are uncorrelated — runs alongside the planner and reviewer with
//! one job: disagree productively. It hits three points in the lifecycle:
//!
//! 1. **After planning** — "what would break this plan?" Risks above a
//!    threshold become guardrail tasks appended to the plan.
//! 2. **After each task review** — independent re-review focused on what
//!    the primary reviewer most often misses (security, perf, data loss).
//! 3. **Before auto-merge** — a final pass; any *high*-severity finding
//!    vetoes the merge regardless of E8's configured gate.
//!
//! Points 1 and 3 are wired: `wingman pilot run` sends the plan through
//! [`review_plan`] and appends [`append_guardrails`]'s tasks before the
//! approval gate, and the pipeline runs the pre-merge pass. Point 2 is not
//! built; the E7 per-task reviewer is the only review a task gets.
//!
//! This module is the decision core: parse the critic's structured output,
//! apply the veto / guardrail rules, and tell model families apart
//! ([`model_family`]) so `[pilot].critic_other_family` can refuse a critic
//! that shares the workers' family. The CLI picks the critic's model.

use serde::{Deserialize, Serialize};

use crate::model::Role;
use crate::planner::{PlannedTask, PlannerLlm};
use crate::severity::{max_severity, Severity};

/// Most guardrail tasks one plan review may add, highest severity first. A
/// critic that lists a dozen medium risks should not double the plan's cost.
pub const MAX_GUARDRAILS: usize = 3;

/// The critic's hard veto threshold for auto-merge. Independent of (and
/// stricter than) E8's `auto_merge_max_severity` — the critic's whole
/// point is to override the primary path's risk tolerance.
pub const VETO_THRESHOLD: Severity = Severity::High;

/// Risks at or above this become guardrail tasks appended to the plan.
pub const GUARDRAIL_THRESHOLD: Severity = Severity::Medium;

/// One risk the critic raised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Risk {
    pub severity: String,
    /// What could break.
    pub description: String,
    /// Optional concrete mitigation — becomes a guardrail task's goal.
    #[serde(default)]
    pub mitigation: Option<String>,
}

impl Risk {
    pub fn severity(&self) -> Severity {
        self.severity.parse().unwrap_or(Severity::Medium)
    }
}

/// The critic agent's structured output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriticReport {
    #[serde(default)]
    pub risks: Vec<Risk>,
    #[serde(default)]
    pub summary: String,
}

/// A guardrail task the critic recommends inserting into the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardrailTask {
    pub title: String,
    pub goal: String,
    pub severity: Severity,
}

impl CriticReport {
    pub fn max_severity(&self) -> Option<Severity> {
        max_severity(&self.risks, Risk::severity)
    }

    /// True when the critic should veto auto-merge: any risk at or above
    /// [`VETO_THRESHOLD`].
    pub fn vetoes_auto_merge(&self) -> bool {
        self.risks.iter().any(|r| r.severity() >= VETO_THRESHOLD)
    }

    /// Risks at or above [`GUARDRAIL_THRESHOLD`], converted into tasks the
    /// planner appends. A risk without a mitigation still produces a task
    /// ("investigate and address: …") so it isn't silently dropped.
    pub fn guardrail_tasks(&self) -> Vec<GuardrailTask> {
        self.risks
            .iter()
            .filter(|r| r.severity() >= GUARDRAIL_THRESHOLD)
            .map(|r| {
                let goal = r
                    .mitigation
                    .clone()
                    .unwrap_or_else(|| format!("Investigate and address: {}", r.description));
                GuardrailTask {
                    title: format!("[guardrail] {}", truncate(&r.description, 60)),
                    goal,
                    severity: r.severity(),
                }
            })
            .collect()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Parse the critic agent's JSON output.
pub fn parse_critic(json: &str) -> Result<CriticReport, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid critic report: {e}"))
}

/// Point 1 — ask the critic what would break `plan`. `None` when the call
/// fails or the reply is not a report; the run then goes on without
/// guardrails, as it would with the critic off.
pub async fn review_plan(
    llm: &dyn PlannerLlm,
    goal: &str,
    plan: &[PlannedTask],
) -> Option<CriticReport> {
    const SYSTEM: &str = "You are an adversarial critic reviewing a plan written by a model \
        from a different family. Name what would break it: missing steps, unsafe ordering, \
        data loss, security, migrations without a way back. Give each risk a concrete \
        mitigation a developer could carry out as one task. Reply with ONLY a JSON object: \
        {\"summary\":\"...\",\"risks\":[{\"severity\":\"low|medium|high|critical\", \
        \"description\":\"...\",\"mitigation\":\"...\"}]}.";
    let tasks = serde_json::to_string_pretty(plan).unwrap_or_default();
    let user = format!("Goal: {goal}\n\nPlan:\n{tasks}");
    let raw = match llm.complete(SYSTEM.into(), user).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(target: "pilot::critic", error = %e, "plan critic call failed");
            return None;
        }
    };
    let json = crate::planner::extract_json_object(&raw)?;
    parse_critic(&json)
        .map_err(|e| tracing::warn!(target: "pilot::critic", "{e}"))
        .ok()
}

/// Append up to [`MAX_GUARDRAILS`] of the report's guardrail tasks to `plan`,
/// highest severity first. Each depends on every task already in the plan, so
/// it runs against the finished work. Returns how many were added.
pub fn append_guardrails(plan: &mut Vec<PlannedTask>, report: &CriticReport) -> usize {
    let mut guardrails = report.guardrail_tasks();
    guardrails.sort_by_key(|g| std::cmp::Reverse(g.severity));
    guardrails.truncate(MAX_GUARDRAILS);
    let deps: Vec<String> = plan.iter().map(|t| t.id.clone()).collect();
    let added = guardrails.len();
    let mut n = 0;
    for g in guardrails {
        let id = loop {
            n += 1;
            let id = format!("guardrail-{n}");
            if plan.iter().all(|t| t.id != id) {
                break id;
            }
        };
        plan.push(PlannedTask {
            id,
            role: Role::Developer,
            title: g.title,
            goal: format!("The plan's critic raised a {} risk. {}", g.severity, g.goal),
            deps: deps.clone(),
            writes: Vec::new(),
            acceptance: Vec::new(),
            reversibility: Default::default(),
            reversibility_reason: None,
        });
    }
    added
}

/// The family a model id belongs to, from its name (`provider/` prefixes and
/// all), or `None` when the name is not one this recognises.
///
/// ponytail: a name table, so a fine-tune or a local model under a custom name
/// is unknown; `critic_other_family` refuses unknown names rather than guess.
pub fn model_family(model: &str) -> Option<&'static str> {
    let name = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    const CONTAINS: &[(&str, &str)] = &[
        ("claude", "anthropic"),
        ("gpt", "openai"),
        ("codex", "openai"),
        ("gemini", "google"),
        ("gemma", "google"),
        ("llama", "meta"),
        ("mistral", "mistral"),
        ("mixtral", "mistral"),
        ("codestral", "mistral"),
        ("devstral", "mistral"),
        ("deepseek", "deepseek"),
        ("qwen", "qwen"),
        ("qwq", "qwen"),
        ("grok", "xai"),
        ("kimi", "moonshot"),
        ("glm", "zhipu"),
    ];
    const PREFIX: &[(&str, &str)] = &[
        ("o1", "openai"),
        ("o3", "openai"),
        ("o4", "openai"),
        ("phi", "microsoft"),
        ("command", "cohere"),
    ];
    CONTAINS
        .iter()
        .find(|(needle, _)| name.contains(needle))
        .or_else(|| PREFIX.iter().find(|(p, _)| name.starts_with(p)))
        .map(|&(_, family)| family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(sev: &str, desc: &str, mit: Option<&str>) -> Risk {
        Risk {
            severity: sev.into(),
            description: desc.into(),
            mitigation: mit.map(String::from),
        }
    }

    #[test]
    fn high_risk_vetoes_auto_merge() {
        let r = CriticReport {
            risks: vec![risk("high", "drops the users table without a backup", None)],
            summary: String::new(),
        };
        assert!(r.vetoes_auto_merge());
    }

    #[test]
    fn medium_risk_does_not_veto() {
        let r = CriticReport {
            risks: vec![risk("medium", "could be slow on large inputs", None)],
            summary: String::new(),
        };
        assert!(!r.vetoes_auto_merge());
    }

    #[test]
    fn critical_risk_vetoes() {
        let r = CriticReport {
            risks: vec![risk("critical", "RCE in the new endpoint", None)],
            summary: String::new(),
        };
        assert!(r.vetoes_auto_merge());
    }

    #[test]
    fn clean_report_does_not_veto() {
        let r = CriticReport {
            risks: vec![],
            summary: "looks robust".into(),
        };
        assert!(!r.vetoes_auto_merge());
        assert!(r.guardrail_tasks().is_empty());
    }

    #[test]
    fn guardrail_tasks_include_medium_and_above() {
        let r = CriticReport {
            risks: vec![
                risk("low", "minor style", None),
                risk("medium", "no rollback path", Some("add a down-migration")),
                risk("high", "no auth check", None),
            ],
            summary: String::new(),
        };
        let tasks = r.guardrail_tasks();
        assert_eq!(tasks.len(), 2); // medium + high, not low
        assert_eq!(tasks[0].goal, "add a down-migration");
        // High risk without mitigation gets a synthesised goal.
        assert!(tasks[1].goal.starts_with("Investigate and address:"));
    }

    #[test]
    fn guardrail_title_is_truncated() {
        let long = "a".repeat(200);
        let r = CriticReport {
            risks: vec![risk("high", &long, None)],
            summary: String::new(),
        };
        let tasks = r.guardrail_tasks();
        assert!(tasks[0].title.chars().count() <= "[guardrail] ".len() + 60);
    }

    #[test]
    fn unknown_severity_defaults_to_medium() {
        assert_eq!(risk("weird", "x", None).severity(), Severity::Medium);
    }

    #[test]
    fn parse_critic_reads_json() {
        let json = r#"{
            "summary": "two concerns",
            "risks": [
                {"severity": "high", "description": "no input validation", "mitigation": "validate at the boundary"},
                {"severity": "low", "description": "naming nit"}
            ]
        }"#;
        let r = parse_critic(json).unwrap();
        assert_eq!(r.risks.len(), 2);
        assert!(r.vetoes_auto_merge());
        assert_eq!(r.max_severity(), Some(Severity::High));
        assert_eq!(r.guardrail_tasks().len(), 1);
    }

    #[test]
    fn parse_critic_rejects_garbage() {
        assert!(parse_critic("nope").is_err());
    }

    /* ── plan-time critic ───────────────────────────────────────────────── */

    struct Canned(Result<String, ()>);

    #[async_trait::async_trait]
    impl PlannerLlm for Canned {
        async fn complete(
            &self,
            system: String,
            user: String,
        ) -> Result<String, wingman_core::WingmanError> {
            assert!(system.contains("adversarial critic"));
            assert!(user.contains("\"id\": \"t1\""), "the plan is shown: {user}");
            self.0
                .clone()
                .map_err(|_| wingman_core::WingmanError::Other("offline".into()))
        }
    }

    fn planned(id: &str) -> PlannedTask {
        PlannedTask {
            id: id.into(),
            role: Role::Developer,
            title: id.into(),
            goal: String::new(),
            deps: Vec::new(),
            writes: vec!["src/lib.rs".into()],
            acceptance: Vec::new(),
            reversibility: Default::default(),
            reversibility_reason: None,
        }
    }

    #[tokio::test]
    async fn review_plan_reads_the_report_and_survives_a_bad_reply() {
        let plan = vec![planned("t1")];
        let reply = r#"Here you go: {"summary":"one","risks":[{"severity":"high","description":"no down-migration","mitigation":"add one"}]}"#;
        let report = review_plan(&Canned(Ok(reply.into())), "goal", &plan)
            .await
            .unwrap();
        assert_eq!(report.risks.len(), 1);
        assert!(review_plan(&Canned(Ok("no json".into())), "goal", &plan)
            .await
            .is_none());
        assert!(review_plan(&Canned(Err(())), "goal", &plan).await.is_none());
    }

    #[test]
    fn guardrails_run_after_the_plan_capped_and_worst_first() {
        let mut plan = vec![planned("t1"), planned("guardrail-1")];
        let report = CriticReport {
            risks: vec![
                risk("medium", "m1", None),
                risk("low", "ignored", None),
                risk("critical", "c1", Some("add auth")),
                risk("medium", "m2", None),
                risk("high", "h1", None),
            ],
            summary: String::new(),
        };
        assert_eq!(append_guardrails(&mut plan, &report), MAX_GUARDRAILS);
        let added = &plan[2..];
        let ids: Vec<&str> = added.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["guardrail-2", "guardrail-3", "guardrail-4"]);
        assert!(
            added[0].goal.contains("critical risk. add auth"),
            "{}",
            added[0].goal
        );
        assert!(added[1].title.contains("h1"));
        assert!(added[2].title.contains("m1"));
        assert!(added.iter().all(|t| t.deps == ["t1", "guardrail-1"]));
        crate::planner::validate_plan(&plan).unwrap();
    }

    #[test]
    fn model_family_reads_the_name_through_provider_prefixes() {
        assert_eq!(model_family("anthropic/claude-opus-4-7"), Some("anthropic"));
        assert_eq!(
            model_family("openrouter/anthropic/claude-sonnet-4"),
            Some("anthropic")
        );
        assert_eq!(model_family("gpt-5-mini"), Some("openai"));
        assert_eq!(model_family("o3-pro"), Some("openai"));
        assert_eq!(model_family("google/gemini-2.5-pro"), Some("google"));
        assert_eq!(model_family("ollama/llama3.2"), Some("meta"));
        assert_eq!(model_family("deepseek/deepseek-chat"), Some("deepseek"));
        assert_eq!(model_family("my-finetune"), None);
    }
}
