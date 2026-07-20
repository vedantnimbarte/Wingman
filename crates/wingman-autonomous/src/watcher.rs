//! J13 — always-on watcher (reactive subset of J2).
//!
//! Where J2's daemon *polls*, the watcher *reacts*: filesystem watcher +
//! git hooks + webhook listener mapping specific high-value events to
//! immediate actions with sub-second latency. The listeners are I/O; this
//! module is the event → reaction mapping.

/// A reactive event the watcher observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A test started failing on the default branch.
    FailingTestOnMain { test: String },
    /// A dependabot PR went green; `within_allowlist` is whether its diff
    /// stays inside the auto-mergeable path allowlist.
    DependabotPrGreen { within_allowlist: bool },
    /// A new issue arrived with the auto label; `trusted` reflects author
    /// trust (J3).
    NewLabeledIssue { trusted: bool },
    /// A `// ASK: <question>` comment was saved in a source file.
    FileSaveAsk { question: String },
}

/// What the watcher should do in response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchReaction {
    /// Spin up a fixer run for the named failure.
    SpawnFixerRun(String),
    /// Auto-review then auto-merge (dependabot within allowlist).
    AutoReviewAndMerge,
    /// Triage immediately rather than waiting for the next poll.
    TriageNow,
    /// Propose to the user (not trusted enough to act).
    Propose,
    /// Spawn a quick research worker to answer an inline question.
    SpawnResearchWorker(String),
    /// Nothing actionable.
    Ignore,
}

/// Map an event to a reaction.
pub fn react(event: &WatchEvent) -> WatchReaction {
    match event {
        WatchEvent::FailingTestOnMain { test } => WatchReaction::SpawnFixerRun(test.clone()),
        WatchEvent::DependabotPrGreen { within_allowlist } => {
            if *within_allowlist {
                WatchReaction::AutoReviewAndMerge
            } else {
                WatchReaction::Propose
            }
        }
        WatchEvent::NewLabeledIssue { trusted } => {
            if *trusted {
                WatchReaction::TriageNow
            } else {
                WatchReaction::Propose
            }
        }
        WatchEvent::FileSaveAsk { question } => {
            if question.trim().is_empty() {
                WatchReaction::Ignore
            } else {
                WatchReaction::SpawnResearchWorker(question.clone())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Listener adapters (the I/O legs feed these; each is pure + testable).
// ---------------------------------------------------------------------------

/// Webhook leg — map a GitHub webhook JSON payload to a [`WatchEvent`].
/// Transport reuses the dependency-free HMAC-authenticated listener in
/// `webhook.rs`; this is just the body parser (the twin of J3's
/// `extract_goal_fields`). Returns `None` for payloads we don't act on.
///
/// Rules:
/// - a `pull_request` opened by dependabot → [`WatchEvent::DependabotPrGreen`]
///   with `within_allowlist` = it carries one of `trusted_labels`.
/// - an `issue` carrying one of `trusted_labels` →
///   [`WatchEvent::NewLabeledIssue`] with `trusted` = its author is in
///   `trusted_authors`.
pub fn parse_github_event(
    payload: &serde_json::Value,
    trusted_authors: &[String],
    trusted_labels: &[String],
) -> Option<WatchEvent> {
    let has_trusted_label = |labels: &serde_json::Value| -> bool {
        labels
            .as_array()
            .map(|arr| {
                arr.iter().any(|l| {
                    l.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| trusted_labels.iter().any(|t| t == n))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };

    if let Some(pr) = payload.get("pull_request") {
        let author = pr
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|l| l.as_str())
            .unwrap_or("");
        if author.contains("dependabot") {
            return Some(WatchEvent::DependabotPrGreen {
                within_allowlist: has_trusted_label(
                    pr.get("labels").unwrap_or(&serde_json::Value::Null),
                ),
            });
        }
        return None;
    }

    if let Some(issue) = payload.get("issue") {
        if has_trusted_label(issue.get("labels").unwrap_or(&serde_json::Value::Null)) {
            let author = issue
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|l| l.as_str())
                .unwrap_or("");
            return Some(WatchEvent::NewLabeledIssue {
                trusted: trusted_authors.iter().any(|a| a == author),
            });
        }
    }
    None
}

/// fs-watch leg — extract the first `// ASK: <question>` (or `# ASK:`) marker
/// from a saved file's contents, if any. The fs-watch transport (a debounced
/// `notify` loop) calls this on each changed file; a hit becomes a
/// [`WatchEvent::FileSaveAsk`].
///
/// ponytail: a substring scan, not a full comment parser — matches `ASK:` in
/// any `//` or `#` line. Add language-aware parsing only if false positives in
/// string literals become a problem.
pub fn scan_file_for_ask(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let t = line.trim_start();
        let Some(body) = t
            .strip_prefix("//")
            .or_else(|| t.strip_prefix('#'))
            .map(str::trim_start)
        else {
            continue; // not a comment line — keep scanning
        };
        if let Some(q) = body.strip_prefix("ASK:") {
            let q = q.trim();
            if !q.is_empty() {
                return Some(q.to_string());
            }
        }
    }
    None
}

/// git-hook leg — install a git hook that POSTs to the watcher's webhook so a
/// local git event (e.g. a failing test after `post-merge`) reaches
/// [`parse_github_event`] over the same HTTP path. Returns the hook path.
/// The hook is a tiny `curl` script — no bespoke IPC socket.
pub fn install_git_hook(
    repo_root: &std::path::Path,
    hook_name: &str,
    webhook_url: &str,
) -> std::io::Result<std::path::PathBuf> {
    let hooks = repo_root.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks)?;
    let path = hooks.join(hook_name);
    let script = format!(
        "#!/bin/sh\n# wingman J13 watcher hook — POSTs the event to the pilot watcher.\n\
         curl -sS -X POST -H 'Content-Type: application/json' \\\n  \
         -d \"{{\\\"hook\\\":\\\"{hook_name}\\\"}}\" {webhook_url} >/dev/null 2>&1 || true\n"
    );
    std::fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failing_test_spawns_fixer() {
        let e = WatchEvent::FailingTestOnMain {
            test: "test_auth".into(),
        };
        assert_eq!(react(&e), WatchReaction::SpawnFixerRun("test_auth".into()));
    }

    #[test]
    fn dependabot_within_allowlist_auto_merges() {
        assert_eq!(
            react(&WatchEvent::DependabotPrGreen {
                within_allowlist: true
            }),
            WatchReaction::AutoReviewAndMerge
        );
    }

    #[test]
    fn dependabot_outside_allowlist_proposes() {
        assert_eq!(
            react(&WatchEvent::DependabotPrGreen {
                within_allowlist: false
            }),
            WatchReaction::Propose
        );
    }

    #[test]
    fn trusted_issue_triages_now() {
        assert_eq!(
            react(&WatchEvent::NewLabeledIssue { trusted: true }),
            WatchReaction::TriageNow
        );
    }

    #[test]
    fn untrusted_issue_proposes() {
        assert_eq!(
            react(&WatchEvent::NewLabeledIssue { trusted: false }),
            WatchReaction::Propose
        );
    }

    #[test]
    fn ask_comment_spawns_research() {
        let e = WatchEvent::FileSaveAsk {
            question: "why is this slow?".into(),
        };
        assert_eq!(
            react(&e),
            WatchReaction::SpawnResearchWorker("why is this slow?".into())
        );
    }

    #[test]
    fn empty_ask_is_ignored() {
        assert_eq!(
            react(&WatchEvent::FileSaveAsk {
                question: "  ".into()
            }),
            WatchReaction::Ignore
        );
    }

    #[test]
    fn j13_parse_github_event_maps_pr_and_issue() {
        use serde_json::json;
        let authors = vec!["vedantnimbarte".to_string()];
        let labels = vec!["wingman:auto".to_string()];

        // dependabot PR carrying the trusted label → within allowlist
        let pr = json!({"pull_request": {"user": {"login": "dependabot[bot]"},
            "labels": [{"name": "wingman:auto"}]}});
        assert_eq!(
            parse_github_event(&pr, &authors, &labels),
            Some(WatchEvent::DependabotPrGreen {
                within_allowlist: true
            })
        );
        // dependabot PR without the label → not allowlisted
        let pr2 = json!({"pull_request": {"user": {"login": "dependabot[bot]"}, "labels": []}});
        assert_eq!(
            parse_github_event(&pr2, &authors, &labels),
            Some(WatchEvent::DependabotPrGreen {
                within_allowlist: false
            })
        );
        // labeled issue from a trusted author → trusted
        let issue = json!({"issue": {"user": {"login": "vedantnimbarte"},
            "labels": [{"name": "wingman:auto"}]}});
        assert_eq!(
            parse_github_event(&issue, &authors, &labels),
            Some(WatchEvent::NewLabeledIssue { trusted: true })
        );
        // unlabeled issue → nothing
        let issue2 = json!({"issue": {"user": {"login": "x"}, "labels": []}});
        assert_eq!(parse_github_event(&issue2, &authors, &labels), None);
    }

    #[test]
    fn j13_scan_file_for_ask_finds_marker() {
        assert_eq!(
            scan_file_for_ask("let x = 1;\n  // ASK: is this right?\nfn f(){}"),
            Some("is this right?".to_string())
        );
        assert_eq!(
            scan_file_for_ask("# ASK: py style?"),
            Some("py style?".to_string())
        );
        assert_eq!(scan_file_for_ask("// just a comment"), None);
        assert_eq!(scan_file_for_ask("// ASK:   "), None);
    }

    #[test]
    fn j13_install_git_hook_writes_executable_script() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let p = install_git_hook(repo.path(), "post-merge", "http://localhost:9099/hook").unwrap();
        assert!(p.ends_with(".git/hooks/post-merge"));
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("curl") && body.contains("localhost:9099"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "hook must be executable");
        }
    }
}
