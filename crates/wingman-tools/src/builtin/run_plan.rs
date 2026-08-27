//! `run_plan`: run a short chain of tool calls in one round trip, where a
//! later call's arguments come from an earlier call's output.
//!
//! # Why this is not "Code Mode"
//!
//! The proposal this came from (P12 in `docs/DSH-ADOPTION.md`) described an
//! embedded JavaScript runtime and a generated SDK, so the model could write a
//! program instead of emitting tool-call JSON. Measuring Wingman first shrank
//! the problem that was meant to solve.
//!
//! The agent loop already emits many tool calls per assistant message and runs
//! the read-only ones concurrently (`AgentConfig::parallel_safe_tools`). So
//! *independent* work already costs one round trip, and a language runtime
//! would buy nothing there. What the loop cannot express is a **dependent**
//! chain — the arguments of call 2 are in the output of call 1 — because the
//! model must see call 1's result before it can write call 2.
//!
//! That gap is this tool's entire scope, and it does not need a language: a
//! list of steps and one substitution rule covers it. Skipped along with the
//! runtime are arithmetic, conditionals, unbounded loops, and every
//! sandbox-escape question an embedded interpreter would have raised.
//!
//! # Security
//!
//! Every call goes back through [`ToolDispatcher::dispatch`], never
//! `Tool::run`. That is the whole security argument, and the reason this is a
//! small change rather than a new trust boundary: dispatch is where the
//! capability gate, the pre/post hooks, the undo checkpoints, the audit trail,
//! secret redaction, the repeat guard and the per-call deadline live. A step
//! that writes is gated exactly as the same call would be on its own, so a
//! plan cannot reach past the session's permission mode.
//!
//! Termination is structural rather than a timeout: a plan is a fixed list,
//! fan-out is capped ([`MAX_CALLS`]), and there is no branching or recursion.

use std::sync::Weak;

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolDispatcher, ToolOutcome, ToolSpec};

/// Steps per plan. A longer chain is a sign the model should look at what it
/// got before committing to the rest.
pub const MAX_STEPS: usize = 8;
/// Total tool calls per plan, across all steps. Bounds `for_each` fan-out.
pub const MAX_CALLS: usize = 32;
/// Per-call output kept in the combined result. Without this, one large file
/// crowds out the other 31 results before the turn's output budget sees them.
pub const MAX_CALL_CHARS: usize = 4_000;

/// The placeholder replaced with the current `for_each` item.
const SLOT: &str = "{}";

pub struct RunPlan {
    /// Weak so the tool can live in the registry it dispatches through
    /// without forming an `Arc` cycle that leaks the registry.
    dispatcher: Weak<dyn ToolDispatcher>,
}

impl RunPlan {
    pub fn new(dispatcher: Weak<dyn ToolDispatcher>) -> Self {
        Self { dispatcher }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    tool: String,
    #[serde(default)]
    args: Value,
    /// Run this step once per value pulled out of an earlier step's output.
    #[serde(default)]
    for_each: Option<ForEach>,
}

#[derive(Debug, Deserialize)]
struct ForEach {
    /// Index of an earlier step, 0-based.
    step: usize,
    /// Regex with exactly one capture group, applied per line of that step's
    /// output. Captures are de-duplicated, keeping first-seen order.
    capture: String,
}

/// Replace every `{}` in every string in `v` with `item`.
fn substitute(v: &Value, item: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace(SLOT, item)),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, item)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, val)| (k.clone(), substitute(val, item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Distinct capture-group values in `text`, in first-seen order.
fn captures(text: &str, re: &regex::Regex) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(m) = re.captures(line).and_then(|c| c.get(1)) {
            let v = m.as_str().to_string();
            if !seen.contains(&v) {
                seen.push(v);
            }
        }
    }
    seen
}

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_CALL_CHARS {
        return s.to_string();
    }
    let kept: String = s.chars().take(MAX_CALL_CHARS).collect();
    format!("{kept}\n… [truncated at {MAX_CALL_CHARS} chars by run_plan]")
}

const DESCRIPTION: &str = "Run a short chain of tool calls in ONE round trip, where a later \
call's arguments come from an earlier call's output.\n\n\
Use this ONLY when a call depends on an earlier call's result. Independent calls are already \
batched — emit them together as separate tool calls, which is cheaper and runs in parallel.\n\n\
Each step names a `tool` and its `args`. Add `for_each` to run a step once per value found in \
an earlier step's output: `step` is that step's index (0-based) and `capture` is a regex with \
ONE capture group applied per line; distinct captures are used, in order. In a `for_each` step, \
`{}` inside any string argument is replaced with the current value.\n\n\
Example — read every file grep matched, without a second round trip:\n\
  step 0: grep with pattern \"TurnGate\"\n\
  step 1: read_file with path \"{}\", for_each {step: 0, capture: \"^([^:]+):\"}\n\n\
Steps run in order. Each call is permission-checked exactly as it would be on its own, so a \
plan cannot do what the current mode forbids.";

#[async_trait]
impl Tool for RunPlan {
    // NONE, deliberately. A plan may do whatever its *steps* may do, and each
    // step is gated individually inside `dispatch`. Declaring the union here
    // would instead fail the whole plan closed in read-only mode even when
    // every step in it was a read.
    fn capabilities(&self) -> Capability {
        Capability::NONE
    }

    // Bounded by MAX_CALLS rather than by a clock. Each inner call still gets
    // the registry's own per-call deadline, so the plan terminates; wrapping
    // the whole plan in one 120s backstop would kill a legitimate chain
    // partway and leave its earlier writes applied.
    fn owns_timeout(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_plan".into(),
            description: format!(
                "{DESCRIPTION}\n\nLimits: {MAX_STEPS} steps, {MAX_CALLS} calls total, \
                 {MAX_CALL_CHARS} chars kept per call."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Steps, run in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string", "description": "Tool to call." },
                                "args": { "type": "object", "description": "Arguments for it." },
                                "for_each": {
                                    "type": "object",
                                    "description": "Repeat this step once per value from an earlier step.",
                                    "properties": {
                                        "step": {
                                            "type": "integer",
                                            "description": "Earlier step index, 0-based."
                                        },
                                        "capture": {
                                            "type": "string",
                                            "description": "Regex with one capture group, applied per line."
                                        }
                                    },
                                    "required": ["step", "capture"],
                                    "additionalProperties": false
                                }
                            },
                            "required": ["tool"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["steps"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        if args.steps.is_empty() {
            return ToolOutcome::err("no steps given");
        }
        if args.steps.len() > MAX_STEPS {
            return ToolOutcome::err(format!(
                "{} steps exceeds the {MAX_STEPS}-step limit; run the first few and look at \
                 what comes back before planning the rest",
                args.steps.len()
            ));
        }
        // The registry outlives the session in practice; a dead weak pointer
        // means teardown, and running half a plan then is worse than refusing.
        let Some(dispatcher) = self.dispatcher.upgrade() else {
            return ToolOutcome::err("tool registry is gone; cannot run a plan");
        };

        let mut outputs: Vec<String> = Vec::with_capacity(args.steps.len());
        let mut report = String::new();
        let mut calls = 0usize;

        for (i, step) in args.steps.iter().enumerate() {
            // No recursion: a plan that can call itself is an unbounded loop
            // wearing a step list, and every argument for bounding this tool
            // structurally depends on it not nesting.
            if step.tool == "run_plan" {
                return ToolOutcome::err("a plan cannot contain `run_plan`");
            }

            // Resolve the values this step runs over. `None` = run it once.
            let items: Option<Vec<String>> = match &step.for_each {
                None => None,
                Some(fe) => {
                    if fe.step >= i {
                        return ToolOutcome::err(format!(
                            "step {i}: for_each.step must name an EARLIER step (got {})",
                            fe.step
                        ));
                    }
                    let re = match regex::Regex::new(&fe.capture) {
                        Ok(re) => re,
                        Err(e) => {
                            return ToolOutcome::err(format!("step {i}: bad capture regex: {e}"))
                        }
                    };
                    if re.captures_len() < 2 {
                        return ToolOutcome::err(format!(
                            "step {i}: capture regex needs one capture group, e.g. `^([^:]+):`"
                        ));
                    }
                    Some(captures(&outputs[fe.step], &re))
                }
            };

            match items {
                None => {
                    if calls >= MAX_CALLS {
                        report.push_str(&format!(
                            "\n[stopped at step {i}: {MAX_CALLS}-call limit]\n"
                        ));
                        break;
                    }
                    calls += 1;
                    let outcome = dispatcher.dispatch(&step.tool, step.args.clone()).await;
                    report.push_str(&format!(
                        "=== step {i}: {}{} ===\n{}\n",
                        step.tool,
                        if outcome.is_error { " (error)" } else { "" },
                        clip(&outcome.content)
                    ));
                    let failed = outcome.is_error;
                    outputs.push(outcome.content);
                    // Later steps usually read this one's output, so carrying
                    // on would fan out over an error message.
                    if failed {
                        report.push_str(&format!(
                            "\n[plan stopped: step {i} failed; later steps were not run]\n"
                        ));
                        break;
                    }
                }
                Some(items) => {
                    if items.is_empty() {
                        report.push_str(&format!(
                            "=== step {i}: {} (for_each matched nothing) ===\n",
                            step.tool
                        ));
                        outputs.push(String::new());
                        continue;
                    }
                    let run_n = items.len().min(MAX_CALLS.saturating_sub(calls));
                    let mut joined = String::new();
                    for item in items.iter().take(run_n) {
                        calls += 1;
                        let outcome = dispatcher
                            .dispatch(&step.tool, substitute(&step.args, item))
                            .await;
                        report.push_str(&format!(
                            "=== step {i}: {} [{item}]{} ===\n{}\n",
                            step.tool,
                            if outcome.is_error { " (error)" } else { "" },
                            clip(&outcome.content)
                        ));
                        joined.push_str(&outcome.content);
                        joined.push('\n');
                        // One bad item does not sink the rest: unlike a single
                        // step, the others do not depend on it.
                    }
                    outputs.push(joined);
                    if run_n < items.len() {
                        report.push_str(&format!(
                            "\n[step {i}: ran {run_n} of {} matches; {MAX_CALLS}-call limit \
                             reached. Narrow the earlier step or split the plan.]\n",
                            items.len()
                        ));
                        break;
                    }
                }
            }
        }

        report.push_str(&format!("\n[run_plan: {calls} call(s)]"));
        ToolOutcome::ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_fills_every_string_slot() {
        let v = json!({"path": "{}", "n": 1, "xs": ["a{}", "b"]});
        assert_eq!(
            substitute(&v, "src/x.rs"),
            json!({"path": "src/x.rs", "n": 1, "xs": ["asrc/x.rs", "b"]})
        );
    }

    #[test]
    fn captures_dedupe_and_keep_order() {
        let re = regex::Regex::new("^([^:]+):").unwrap();
        // grep-shaped output: the same file matched three times.
        let text = "b.rs:1:x\na.rs:2:y\nb.rs:9:z\n";
        assert_eq!(captures(text, &re), vec!["b.rs", "a.rs"]);
    }

    #[test]
    fn clip_bounds_one_calls_output() {
        let big = "x".repeat(MAX_CALL_CHARS * 2);
        let out = clip(&big);
        assert!(out.contains("truncated"));
        assert!(out.chars().count() < MAX_CALL_CHARS + 100);
        // Under the cap, text is returned untouched.
        assert_eq!(clip("short"), "short");
    }
}
