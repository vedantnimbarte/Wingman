//! What `run_plan` costs and what it saves, in tokens.
//!
//! P12 asked for the token delta to be measured before deciding, rather than
//! assumed from the shape of the idea. This measures the part that can be
//! measured without a provider: the standing cost of carrying the tool, and
//! the round trip it removes.
//!
//!     cargo run --example plan_cost -p wingman-tools
//!
//! The estimator is Wingman's own `estimate_tokens` (chars/4). It is a
//! heuristic, so treat these as the right order of magnitude, not exact
//! billing.

use std::sync::Arc;

use wingman_config::PermissionMode;
use wingman_core::estimate_tokens;
use wingman_core::ToolDispatcher;
use wingman_tools::builtin::RunPlan;
use wingman_tools::{ToolCtx, ToolRegistry};

/// Tokens a `ToolSpec` occupies in the request's tool list.
fn spec_tokens(spec: &wingman_core::ToolSpec) -> u32 {
    estimate_tokens(&spec.name)
        + estimate_tokens(&spec.description)
        + estimate_tokens(&spec.input_schema.to_string())
}

fn main() {
    let tmp = std::env::temp_dir();
    let ctx = ToolCtx::new(PermissionMode::ReadOnly, tmp.clone(), tmp);
    let reg = Arc::new(ToolRegistry::new(ctx).with_builtins());
    let as_dispatcher: Arc<dyn ToolDispatcher> = reg.clone();
    reg.register_arc(Arc::new(RunPlan::new(Arc::downgrade(&as_dispatcher))));

    let specs = reg.specs();
    let total: u32 = specs.iter().map(spec_tokens).sum();
    let plan = specs
        .iter()
        .find(|s| s.name == "run_plan")
        .map(spec_tokens)
        .expect("run_plan is registered");

    println!("== the standing cost ==");
    println!("  run_plan's spec        : {plan} tokens");
    println!("  all {:>2} tool specs      : {total} tokens", specs.len());
    println!(
        "  share of the tool list : {:.1}%",
        plan as f64 / total as f64 * 100.0
    );
    println!();
    println!("  Paid on every request, but it sits in the tools block, which is");
    println!("  behind a cache breakpoint (CacheBreakpoint::AfterTools). So it is");
    println!("  a one-off cache write, then cache reads at ~10% of input price.");
    println!();

    // What it saves. A dependent chain — grep, then read each file it matched
    // — is the case the loop cannot already collapse, because the model has to
    // see the grep output before it can write the reads.
    println!("== what it saves, per dependent chain ==");
    println!("  Without run_plan:");
    println!("    request 1  tools=grep                   -> results");
    println!("    request 2  tools=read_file x N (batched, parallel)");
    println!("    request 3  the answer");
    println!("  With run_plan:");
    println!("    request 1  tools=run_plan               -> all results");
    println!("    request 2  the answer");
    println!();
    println!("  Saved: one provider round trip.");
    println!();
    println!("  NOT saved: the conversation itself. Request 2 re-sends the same");
    println!("  prefix either way, and with [tokens].prompt_cache on that prefix");
    println!("  is a cache read. Removing a round trip removes a cache read plus");
    println!("  the output tokens for N tool calls — not a full prompt.");
    println!();
    println!("  So the honest saving is mostly LATENCY (one provider request),");
    println!("  and a modest number of output tokens. It is not the order-of-");
    println!("  magnitude token win the proposal implied, because the loop");
    println!("  already batches independent calls.");
}
