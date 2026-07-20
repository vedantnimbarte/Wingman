//! Git-native auto-commit (Aider-style): turn each AI change into a reviewable,
//! revertable commit. Enabled via `[git].auto_commit`. Message generation is a
//! zero-cost heuristic (no model call) derived from the changed files and,
//! optionally, the user's prompt.

use std::path::Path;
use std::process::Command;

/// True if `root` is inside a git work tree.
pub fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Changed (tracked or untracked) paths in the work tree, via
/// `git status --porcelain`. Empty when clean or not a repo.
pub fn changed_paths(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            // "XY <path>" — path starts at column 3.
            let l = l.trim_end();
            if l.len() > 3 {
                Some(l[3..].to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Build a commit subject + body from the changed files and an optional prompt.
pub fn generate_message(prefix: &str, changed: &[String], user_prompt: Option<&str>) -> String {
    // Subject: prefix + a short intent line. Prefer the user's prompt (first
    // line, trimmed) so the history reads intentionally; else summarize files.
    let intent = user_prompt
        .and_then(|p| p.lines().find(|l| !l.trim().is_empty()))
        .map(|l| l.trim())
        .filter(|l| !l.is_empty());

    let subject = match intent {
        Some(i) => format!("{prefix}{}", truncate(i, 68 - prefix.len().min(60))),
        None => {
            let n = changed.len();
            let sample: Vec<&str> = changed.iter().take(3).map(String::as_str).collect();
            let more = if n > 3 {
                format!(" (+{} more)", n - 3)
            } else {
                String::new()
            };
            format!("{prefix}update {n} file(s): {}{more}", sample.join(", "))
        }
    };

    // Body: the file list so the commit is self-describing.
    let mut body = String::from("Changed files:\n");
    for f in changed {
        body.push_str("  - ");
        body.push_str(f);
        body.push('\n');
    }
    body.push_str("\nCommitted automatically by wingman ([git].auto_commit).");
    format!("{subject}\n\n{body}")
}

/// Stage everything and commit with `message`. Returns the new commit's short
/// hash + subject on success, `Ok(None)` if there was nothing to commit, or an
/// error string. Never touches an unclean index destructively beyond `add -A`.
pub fn commit_all(root: &Path, message: &str) -> Result<Option<String>, String> {
    if !is_git_repo(root) {
        return Ok(None);
    }
    if changed_paths(root).is_empty() {
        return Ok(None);
    }
    run(root, &["add", "-A"])?;
    // `git commit` exits non-zero if nothing is staged (e.g. all changes were
    // already committed by a hook); treat that as "nothing to commit".
    let out = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git commit: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("nothing to commit") {
            return Ok(None);
        }
        return Err(format!("git commit failed: {}", err.trim()));
    }
    // Report the new commit.
    let show = Command::new("git")
        .args(["log", "-1", "--pretty=%h %s"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git log: {e}"))?;
    Ok(Some(String::from_utf8_lossy(&show.stdout).trim().to_string()))
}

/// Convenience: if `[git].auto_commit` is on, commit the work-tree changes with
/// a generated message. Returns the commit line for the UI, if one was made.
pub fn auto_commit_if_enabled(
    cfg: &wingman_config::Config,
    root: &Path,
    user_prompt: Option<&str>,
) -> Option<String> {
    if !cfg.git.auto_commit {
        return None;
    }
    let changed = changed_paths(root);
    if changed.is_empty() {
        return None;
    }
    let message = generate_message(&cfg.git.auto_commit_prefix, &changed, user_prompt);
    match commit_all(root, &message) {
        Ok(Some(line)) => Some(line),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("auto-commit failed: {e}");
            None
        }
    }
}

fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_prefers_prompt_intent() {
        let changed = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let m = generate_message("wingman: ", &changed, Some("Add retry to the client\n\nmore"));
        assert!(m.starts_with("wingman: Add retry to the client"));
        assert!(m.contains("src/a.rs"));
        assert!(m.contains("src/b.rs"));
    }

    #[test]
    fn message_falls_back_to_file_summary() {
        let changed = vec!["a".to_string(), "b".to_string(), "c".to_string(), "d".to_string()];
        let m = generate_message("wingman: ", &changed, None);
        assert!(m.contains("update 4 file(s)"));
        assert!(m.contains("(+1 more)"));
    }

    #[test]
    fn commit_all_noops_outside_git() {
        let dir = std::env::temp_dir().join(format!("wm-gitauto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Not a git repo → Ok(None), no error.
        assert_eq!(commit_all(&dir, "x").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
