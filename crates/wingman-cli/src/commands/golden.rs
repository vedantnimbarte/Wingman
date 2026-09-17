//! `wingman golden` — characterization / golden-master testing.
//!
//! Snapshot the *observable behavior* of a command (its output), then fail if a
//! later change alters it. This is the safety net for undertested / legacy code
//! and the "verified correct, not just verified builds" layer: capture the
//! behavior you want to preserve, then let the verification gate catch any
//! unintended change to it — something no other coding agent offers.
//!
//! Goldens live under `<project>/.wingman/golden/<name>.json` as
//! `{ command, output, captured_at }` so `check` can re-run and diff.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use wingman_config::ProjectPaths;

#[derive(Debug, Serialize, Deserialize)]
pub struct Golden {
    /// The shell command whose output is the characterized behavior.
    pub command: String,
    /// The captured output (stdout, or stdout+stderr on failure).
    pub output: String,
}

pub async fn capture(name: String, command: Vec<String>) -> Result<ExitCode> {
    if command.is_empty() {
        eprintln!("wingman: pass a command after `--`, e.g. `wingman golden capture parse -- cargo run -- parse fixtures/x`");
        return Ok(ExitCode::from(1));
    }
    let cmd = command.join(" ");
    let dir = golden_dir()?;
    std::fs::create_dir_all(&dir).ok();
    let output = run(&cmd)?;
    let golden = Golden {
        command: cmd.clone(),
        output,
    };
    let path = dir.join(format!("{}.json", sanitize(&name)));
    std::fs::write(&path, serde_json::to_string_pretty(&golden)?)
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "captured golden `{name}` ({} bytes) → {}",
        golden.output.len(),
        path.display()
    );
    println!("run `wingman golden check {name}` after changes to catch behavior drift.");
    Ok(ExitCode::SUCCESS)
}

/// Re-run stored goldens and diff. `name` limits to one; otherwise checks all.
pub async fn check(name: Option<String>) -> Result<ExitCode> {
    let dir = golden_dir()?;
    let goldens = load_all(&dir, name.as_deref())?;
    if goldens.is_empty() {
        println!("(no goldens captured — use `wingman golden capture <name> -- <cmd>`)");
        return Ok(ExitCode::SUCCESS);
    }
    let mut failed = 0usize;
    for (gname, g) in &goldens {
        match run(&g.command) {
            Ok(current) if current == g.output => println!("  ✓ {gname}"),
            Ok(current) => {
                failed += 1;
                println!("  ✗ {gname}: behavior changed");
                print_diff(&g.output, &current);
            }
            Err(e) => {
                failed += 1;
                println!("  ✗ {gname}: command failed to run: {e}");
            }
        }
    }
    if failed == 0 {
        println!("\nall {} golden(s) unchanged.", goldens.len());
        Ok(ExitCode::SUCCESS)
    } else {
        println!("\n{failed} golden(s) drifted. Re-capture with `wingman golden capture <name> -- <cmd>` if intended.");
        Ok(ExitCode::from(1))
    }
}

pub async fn list() -> Result<ExitCode> {
    let dir = golden_dir()?;
    let goldens = load_all(&dir, None)?;
    if goldens.is_empty() {
        println!("(no goldens)");
    } else {
        for (name, g) in &goldens {
            println!("  {name}: `{}`", g.command);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Load goldens for the gate (public so `runtime`'s BehaviorGate can reuse it).
pub fn load_goldens(root: &Path) -> Vec<(String, Golden)> {
    load_all(&root.join(".wingman").join("golden"), None).unwrap_or_default()
}

/// Run every golden and return the names that drifted (empty = all clean).
/// Used by the verification gate.
pub fn check_all(root: &Path) -> (usize, Vec<String>) {
    let goldens = load_goldens(root);
    let total = goldens.len();
    let mut drifted = Vec::new();
    for (name, g) in goldens {
        match run(&g.command) {
            Ok(current) if current == g.output => {}
            _ => drifted.push(name),
        }
    }
    (total, drifted)
}

fn golden_dir() -> Result<PathBuf> {
    let paths = ProjectPaths::discover(&std::env::current_dir()?);
    Ok(paths.dir.join("golden"))
}

fn load_all(dir: &Path, only: Option<&str>) -> Result<Vec<(String, Golden)>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if let Some(only) = only {
            if name != only {
                continue;
            }
        }
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(g) = serde_json::from_str::<Golden>(&text) {
                out.push((name, g));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Run `cmd` via the shell and return stdout (or stdout+stderr on failure).
fn run(cmd: &str) -> Result<String> {
    let output = wingman_tools::child_process::shell_command(cmd)
        .output()
        .with_context(|| format!("running `{cmd}`"))?;
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        s.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(s)
}

fn print_diff(expected: &str, actual: &str) {
    let ev: Vec<&str> = expected.lines().collect();
    let av: Vec<&str> = actual.lines().collect();
    let n = ev.len().max(av.len());
    let mut shown = 0;
    for i in 0..n {
        if shown >= 20 {
            println!("    … ({} more differing lines)", n - i);
            break;
        }
        match (ev.get(i), av.get(i)) {
            (Some(e), Some(a)) if e == a => {}
            (e, a) => {
                if let Some(e) = e {
                    println!("    - {e}");
                }
                if let Some(a) = a {
                    println!("    + {a}");
                }
                shown += 1;
            }
        }
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize("a/b c"), "a_b_c");
        assert_eq!(sanitize("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn run_captures_output() {
        let out = run("echo hello").unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn load_and_check_all_detects_drift() {
        let dir = std::env::temp_dir().join(format!("wm-golden-{}", std::process::id()));
        let gdir = dir.join(".wingman").join("golden");
        std::fs::create_dir_all(&gdir).unwrap();
        // A golden whose command output won't match the stored (stale) output.
        let g = Golden {
            command: "echo NOW".to_string(),
            output: "OLD\n".to_string(),
        };
        std::fs::write(gdir.join("x.json"), serde_json::to_string(&g).unwrap()).unwrap();
        let (total, drifted) = check_all(&dir);
        assert_eq!(total, 1);
        assert_eq!(drifted, vec!["x".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
