//! `wingman router stats` — show which model wins per task class in this repo:
//! the verification-gate pass-rate recorded per turn (headless, TUI and pilot
//! workers), beside the durable verdicts `wingman router backfill` records
//! for merged pilot PRs. Learned routing (`[router].learned_min_samples`)
//! picks from the same table.

use std::process::ExitCode;

use anyhow::Result;
use wingman_config::ProjectPaths;

use crate::cli::RouterAction;

pub async fn run(action: RouterAction) -> Result<ExitCode> {
    match action {
        RouterAction::Stats { all } => stats(all).await,
        RouterAction::Backfill { days } => backfill(days).await,
        RouterAction::Preset { name, model } => preset(&name, model).await,
    }
}

/// Print a recommended `[router]` preset. The `local` preset keeps the cheap,
/// low-intelligence steps (summarize, compaction, commit-message, title,
/// search) on a local model so "simple steps never leave your machine" — a
/// privacy story a single-vendor agent structurally can't tell.
async fn preset(name: &str, model: Option<String>) -> Result<ExitCode> {
    match name {
        "local" => {
            let m = model.unwrap_or_else(|| "ollama/llama3.1".to_string());
            println!("# Local-first privacy preset. Paste into ~/.wingman/config.toml.");
            println!("# Cheap, low-intelligence steps run on your local model and never");
            println!("# leave the machine; reasoning/codegen stay on your session model.");
            println!();
            println!("[router]");
            println!("local_model = \"{m}\"");
            println!();
            println!("[router.classes]");
            for class in [
                "summarize",
                "search_summarize",
                "compaction",
                "commit_message",
                "title",
            ] {
                println!("{class:<16} = \"local\"");
            }
            println!(
                "{:<16} = \"default\"   # keep real thinking on the session model",
                "reason"
            );
            println!("{:<16} = \"default\"", "codegen");
            println!();
            println!("# Requires a local server running (e.g. `ollama serve`) with the");
            println!("# model pulled. Run `wingman discover` to find local models.");
            Ok(ExitCode::SUCCESS)
        }
        other => {
            eprintln!("wingman: unknown preset '{other}' (available: local)");
            Ok(ExitCode::from(1))
        }
    }
}

async fn stats(all: bool) -> Result<ExitCode> {
    let store = match wingman_learn::StatsStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wingman: no stats db ({e})");
            return Ok(ExitCode::SUCCESS);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = ProjectPaths::discover(&cwd);
    let repo = paths.root.to_string_lossy().to_string();
    let scope = if all { None } else { Some(repo.as_str()) };

    let rows = store.routing_summary(scope)?;
    if rows.is_empty() {
        println!(
            "No routing data yet{}. It accrues as you run sessions with the verification gate on.",
            if all { "" } else { " for this repo" }
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "Routing win-rates{}:",
        if all { " (all repos)" } else { " (this repo)" }
    );
    println!("  gate    = the verification gate passed when the turn ended");
    println!("  durable = merged PRs that held when judged (`wingman router backfill`);");
    println!("            unknown PRs are listed but never counted as held");
    let mut current = String::new();
    for r in &rows {
        if r.task_class != current {
            println!("\nclass: {}", r.task_class);
            current = r.task_class.clone();
        }
        println!("  {}", stat_line(r));
    }
    Ok(ExitCode::SUCCESS)
}

/// One model's row: the gate tally, then the PR verdicts beside it.
fn stat_line(r: &wingman_learn::RoutingStat) -> String {
    let gate = format!(
        "{:<40} {:>3}/{:<3} {:>4.0}% gate",
        r.model,
        r.passed,
        r.total,
        r.pass_rate() * 100.0
    );
    if r.held + r.reverted + r.unknown == 0 {
        return format!("{gate}   no PR verdicts");
    }
    let rate = match r.durable_rate() {
        Some(rate) => format!("{:.0}% durable", rate * 100.0),
        None => "durable: undecided".to_string(),
    };
    format!(
        "{gate}   {rate} ({} held, {} reverted, {} unknown)",
        r.held, r.reverted, r.unknown
    )
}

async fn backfill(days: u32) -> Result<ExitCode> {
    let store = wingman_learn::StatsStore::open_default()?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let lines = backfill_project(
        &store,
        &wingman_autonomous::pr::SystemCommandRunner,
        &project.root,
        days,
        chrono::Utc::now(),
    );
    for line in lines {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Judge every pilot run's PR in `project_root` that has no decided verdict
/// yet, and report what happened to each. A PR too young, still open, or
/// unreadable is left for a later backfill, and one judged `unknown` is judged
/// again each time, so a revert that lands after the first look still counts.
fn backfill_project(
    store: &wingman_learn::StatsStore,
    runner: &dyn wingman_autonomous::pr::CommandRunner,
    project_root: &std::path::Path,
    days: u32,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut waiting = 0;
    for run in wingman_autonomous::dashboard::load_all_run_states(project_root) {
        let Some(pr) = run.pr_url else { continue };
        match store.has_verdict(&pr) {
            Ok(false) => {}
            Ok(true) => continue,
            Err(e) => {
                lines.push(format!("{pr}: cannot read learn.db ({e})"));
                continue;
            }
        }
        let facts = match wingman_autonomous::feedback::durability_facts(
            runner,
            project_root,
            &pr,
            chrono::Duration::days(days.into()),
            now,
        ) {
            Ok(Some(facts)) => facts,
            Ok(None) => {
                waiting += 1;
                continue;
            }
            Err(e) => {
                lines.push(format!("{pr}: not judged ({e})"));
                continue;
            }
        };
        let verdict = facts.verdict().as_str();
        // The worker sessions are what tie the PR to the (role, model) rows
        // their gate results were recorded under.
        let sessions: Vec<String> = run
            .agents
            .iter()
            .filter_map(|a| a.session_id.clone())
            .collect();
        lines.push(match store.record_verdict(&pr, &sessions, verdict) {
            Ok(0) => format!(
                "{pr}: {verdict}, not recorded: none of this run's workers reached a \
                 verification gate, so there is no role and model to record it against"
            ),
            Ok(n) => format!("{pr}: {verdict} (recorded for {n} role/model pair(s))"),
            Err(e) => format!("{pr}: {verdict}, not recorded ({e})"),
        });
    }
    if waiting > 0 {
        lines.push(format!(
            "{waiting} PR(s) not judged yet: still open, closed unmerged, or merged less than {days} day(s) ago."
        ));
    }
    if lines.is_empty() {
        lines.push("No pilot PRs awaiting a verdict.".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use wingman_autonomous::pr::{CommandOut, CommandRunner};

    /// `gh` reports every PR merged 40 days ago and untouched since, so each
    /// judged PR comes out `unknown`; `git` is unavailable.
    struct Gh;
    impl CommandRunner for Gh {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            _: &std::path::Path,
        ) -> std::io::Result<CommandOut> {
            let merged = (chrono::Utc::now() - chrono::Duration::days(40)).to_rfc3339();
            let stdout = match (program, args.first()) {
                ("gh", Some(&"pr")) if args[2].ends_with("/young") => {
                    format!(
                        r#"{{"state":"MERGED","mergedAt":"{}","mergeCommit":{{"oid":"m"}},"baseRefName":"main"}}"#,
                        chrono::Utc::now().to_rfc3339()
                    )
                }
                ("gh", Some(&"pr")) => format!(
                    r#"{{"state":"MERGED","mergedAt":"{merged}","mergeCommit":{{"oid":"m"}},"baseRefName":"main","closingIssuesReferences":[]}}"#
                ),
                _ => {
                    return Ok(CommandOut {
                        status: Some(1),
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                }
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    fn write_run(root: &std::path::Path, id: &str, pr: &str, session: &str) {
        let dir = root.join(".wingman").join("autonomous").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let state = serde_json::json!({
            "run_id": id, "goal": "g", "base_commit": "b", "integration_branch": "i",
            "pr_url": pr,
            "agents": [{"id": "a1", "role": "developer", "status": "done", "session_id": session}],
        });
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
    }

    #[test]
    fn backfill_records_once_and_waits_on_young_prs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let store = wingman_learn::StatsStore::open(&root.join("learn.db")).unwrap();
        store
            .record_routing("developer", "a/opus", "r", Some("w1"), true)
            .unwrap();
        write_run(root, "r1", "https://x/pull/old", "w1");
        write_run(root, "r2", "https://x/pull/young", "w2");
        write_run(root, "r3", "https://x/pull/nogate", "w3");

        let lines = backfill_project(&store, &Gh, root, 30, chrono::Utc::now());
        let text = lines.join("\n");
        assert!(
            text.contains("pull/old: unknown (recorded for 1 role/model pair(s))"),
            "{text}"
        );
        assert!(
            text.contains("pull/nogate: unknown, not recorded"),
            "{text}"
        );
        assert!(text.contains("1 PR(s) not judged yet"), "{text}");
        let stats = store.routing_summary(Some("r")).unwrap();
        assert_eq!(
            (stats[0].held, stats[0].reverted, stats[0].unknown),
            (0, 0, 1)
        );

        // An unknown PR is judged again on the next pass, replacing its
        // verdict rather than adding a second one.
        let again = backfill_project(&store, &Gh, root, 30, chrono::Utc::now()).join("\n");
        assert!(again.contains("pull/old: unknown"), "{again}");
        let stats = store.routing_summary(Some("r")).unwrap();
        assert_eq!(stats[0].unknown, 1);

        // A decided one is final and not judged again.
        store
            .record_verdict("https://x/pull/old", &["w1".to_string()], "held")
            .unwrap();
        let settled = backfill_project(&store, &Gh, root, 30, chrono::Utc::now()).join("\n");
        assert!(!settled.contains("pull/old"), "{settled}");
    }

    #[test]
    fn a_stat_line_names_both_signals() {
        let mut r = wingman_learn::RoutingStat {
            task_class: "default".into(),
            model: "a/opus".into(),
            passed: 3,
            total: 4,
            held: 0,
            reverted: 0,
            unknown: 0,
        };
        assert!(stat_line(&r).ends_with("75% gate   no PR verdicts"));
        r.unknown = 2;
        assert!(stat_line(&r).contains("durable: undecided (0 held, 0 reverted, 2 unknown)"));
        r.held = 3;
        r.reverted = 1;
        assert!(stat_line(&r).contains("75% durable (3 held, 1 reverted, 2 unknown)"));
    }
}
