//! `wingman spec` — intent-first / test-first implementation.
//!
//! Instead of prompt→code, work spec→tests→code: state the intent, the agent
//! writes tests that capture it (failing first), then implements against them.
//!
//! Scope, honestly: this command is a structured prompt plus one headless
//! turn. The `[verify]` gate is what pushes back on a red build, but it is
//! bounded — it feeds failures to the model up to `[verify].max_retries`
//! (default 2) and then stops anyway, reporting `AgentStop::GateFailed` and
//! exiting non-zero. So "until green" means "with a bounded number of forced
//! corrections", not "loops until it passes". Runs in auto-edit.

use anyhow::Result;
use std::process::ExitCode;
use wingman_config::{global_config_path, Config, PermissionMode, ProjectPaths};

pub async fn run(intent: String) -> Result<ExitCode> {
    let prompt = format!(
        "Implement the following intent **test-first**:\n\n\
         INTENT: {intent}\n\n\
         Workflow:\n\
         1. Restate the intent as concrete, checkable acceptance criteria.\n\
         2. Write tests that encode those criteria FIRST — they should fail now \
            because the behavior doesn't exist yet. Run them to confirm they fail \
            for the right reason.\n\
         3. Implement the minimal change to make the tests pass.\n\
         4. Run the tests (and the verification gate). Do NOT declare done until \
            they are green. If a test was wrong, fix the test deliberately and say so.\n\
         5. Summarize what you added and how it's verified.\n\
         Prefer small, focused edits. Keep the public API stable unless the intent requires otherwise."
    );

    let cfg = load_config()?;
    let opts = crate::commands::headless::HeadlessOptions {
        prompt,
        json: false,
        mode_override: Some(PermissionMode::AutoEdit),
        model_override: None,
    };
    crate::commands::headless::run(cfg, opts).await
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project.config_file.exists().then_some(project.config_file);
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}
