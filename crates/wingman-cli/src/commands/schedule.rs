//! `wingman schedule run` — fire any `[[schedule]]` entries whose cadence
//! has elapsed since their last successful run.
//!
//! Last-run timestamps live at `~/.wingman/schedule.json` as
//! `{ "<id>": <unix_secs> }`. Intended for cron-style invocation; we do
//! NOT spawn a daemon.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use wingman_config::{global_config_path, global_dir, Config, ProjectPaths, ScheduledTask};

const STATE_FILE: &str = "schedule.json";

pub async fn run(all: bool) -> Result<ExitCode> {
    let cfg = load_config()?;
    if cfg.schedule.is_empty() {
        eprintln!("wingman: no [[schedule]] entries configured");
        return Ok(ExitCode::SUCCESS);
    }

    let now = unix_now();
    let mut state = load_state();
    let mut ran = 0usize;
    let mut failed = 0usize;
    for task in &cfg.schedule {
        let last = state.get(&task.id).copied().unwrap_or(0);
        let due = all || now.saturating_sub(last) >= task.every_secs;
        if !due {
            continue;
        }
        println!(
            "→ running task '{}' (last ran {}s ago)",
            task.id,
            now - last
        );
        match fire(&cfg, task).await {
            Ok(_) => {
                state.insert(task.id.clone(), now);
                ran += 1;
            }
            Err(e) => {
                eprintln!("  failed: {e}");
                failed += 1;
            }
        }
    }
    if let Err(e) = save_state(&state) {
        eprintln!("wingman: warning, could not save schedule state: {e}");
    }
    println!("\nscheduled tasks: {ran} ran, {failed} failed");
    Ok(if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

async fn fire(cfg: &Config, task: &ScheduledTask) -> Result<()> {
    let opts = crate::commands::headless::HeadlessOptions {
        prompt: task.prompt.clone(),
        json: false,
        mode_override: None,
        model_override: task.model.clone(),
    };
    let code = crate::commands::headless::run(cfg.clone(), opts).await?;
    if code != ExitCode::SUCCESS {
        anyhow::bail!("headless run exited non-zero");
    }
    Ok(())
}

fn load_config() -> Result<Config> {
    let global = global_config_path()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let project_file: Option<PathBuf> = if project.config_file.exists() {
        Some(project.config_file)
    } else {
        None
    };
    Ok(Config::load(Some(&global), project_file.as_deref())?)
}

fn state_path() -> PathBuf {
    global_dir()
        .ok()
        .map(|d| d.join(STATE_FILE))
        .unwrap_or_else(|| PathBuf::from(STATE_FILE))
}

fn load_state() -> BTreeMap<String, u64> {
    let p = state_path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_state(s: &BTreeMap<String, u64>) -> Result<()> {
    let p = state_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
