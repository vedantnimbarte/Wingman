//! R2 — post-merge feedback loop.
//!
//! E6 and J8 claim cross-run learning, but nothing in M1/early-M2 observes
//! what happens to a PR *after* it merges — only what made it through
//! review. Without that signal, "learning" is theater: the agent sees its
//! own in-process outcomes and never production reality.
//!
//! This module closes the loop. It provides:
//!
//! - [`PrState`] + [`parse_pr_view_json`] — parse `gh pr view --json …`
//!   output into a typed state, so a poller can map an open PR to a
//!   [`PrOutcomeKind`].
//! - [`is_revert_of`] / [`detect_reverts`] — recognise
//!   `Revert "<original title>"` commits in a `git log` so a poller can
//!   spot reverts without a webhook.
//! - [`durability_facts`] + [`Durability`] — the durable verdict on a merged
//!   PR some days on (#183): reverted, rewritten, broke the base branch,
//!   reopened its issue, or held. `wingman router backfill` records it.
//! - [`WeightedStats`] — fold a stream of [`PrOutcomeKind`] (per run, per
//!   role, per model) into a weighted score the adaptive router (E6) reads
//!   instead of the raw first-try pass rate.
//!
//! Everything here is pure and I/O-free so it unit-tests without a network
//! or a git repo. The orchestrator/daemon supplies the `gh` / `git`
//! output; this module turns it into events and stats.

use std::path::Path;

use crate::model::PrOutcomeKind;
use crate::pr::CommandRunner;

/// The lifecycle state of a PR as reported by `gh pr view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    /// Closed without merging.
    Closed,
}

/// Outcome of mapping a PR's observed state (+ optional revert/hotfix
/// signal) onto an [`PrOutcomeKind`]. `None` means "nothing terminal yet"
/// (PR still open) — the poller should check again later.
pub fn classify_pr(state: PrState, reverted: bool, hotfix_followed: bool) -> Option<PrOutcomeKind> {
    match state {
        PrState::Open => None,
        PrState::Closed => Some(PrOutcomeKind::Closed),
        PrState::Merged => {
            if reverted {
                Some(PrOutcomeKind::Reverted)
            } else if hotfix_followed {
                Some(PrOutcomeKind::HotfixFollowed)
            } else {
                Some(PrOutcomeKind::Merged)
            }
        }
    }
}

/// Parse the JSON produced by `gh pr view <n> --json state,mergedAt,closed`.
/// `gh` reports `state` as one of `OPEN`, `MERGED`, `CLOSED`. We tolerate
/// case and fall back on the boolean fields when present.
pub fn parse_pr_view_json(json: &str) -> Result<PrState, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid gh json: {e}"))?;
    if let Some(state) = v.get("state").and_then(|s| s.as_str()) {
        return match state.to_ascii_uppercase().as_str() {
            "OPEN" => Ok(PrState::Open),
            "MERGED" => Ok(PrState::Merged),
            "CLOSED" => Ok(PrState::Closed),
            other => Err(format!("unknown gh pr state '{other}'")),
        };
    }
    // Fallback: derive from mergedAt / closed booleans.
    let merged_at = v.get("mergedAt").and_then(|m| m.as_str());
    if merged_at.is_some_and(|s| !s.is_empty()) {
        return Ok(PrState::Merged);
    }
    if v.get("closed").and_then(|c| c.as_bool()) == Some(true) {
        return Ok(PrState::Closed);
    }
    Ok(PrState::Open)
}

/// R2 poller shell: query a PR's terminal state via `gh pr view` and map
/// it to a [`PrOutcomeKind`]. Returns `Ok(None)` while the PR is still
/// open (the daemon re-polls later). Revert/hotfix refinement is a
/// separate `git log` pass ([`detect_reverts`]); this resolves the
/// merged-vs-closed-vs-open question that needs the GitHub API.
pub fn poll_pr_outcome(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr_ref: &str,
) -> Result<Option<PrOutcomeKind>, String> {
    let out = runner
        .run(
            "gh",
            &["pr", "view", pr_ref, "--json", "state,mergedAt,closed"],
            repo_root,
        )
        .map_err(|e| format!("gh pr view failed: {e}"))?;
    if !out.success() {
        return Err(format!("gh pr view exited non-zero: {}", out.stderr.trim()));
    }
    let state = parse_pr_view_json(&out.stdout)?;
    Ok(classify_pr(state, false, false))
}

/// R2 poll-and-record: poll the PR and, if it has reached a terminal
/// state, append a [`crate::model::Event::PrOutcome`] to the run store so
/// the cross-run learning loop (E6) sees it. Returns the recorded outcome
/// (or `None` if the PR is still open). The scheduling cadence that calls
/// this is the daemon's concern.
pub async fn poll_and_record(
    runner: &dyn CommandRunner,
    store: &mut crate::store::RunStore,
    repo_root: &Path,
    pr_ref: &str,
) -> Result<Option<PrOutcomeKind>, String> {
    let outcome = poll_pr_outcome(runner, repo_root, pr_ref)?;
    if let Some(kind) = outcome {
        let run_id = store.state().run_id.clone();
        store
            .append(crate::model::Event::PrOutcome {
                t: crate::store::RunStore::now(),
                run_id,
                kind,
                revert_sha: None,
                hours_to_revert: None,
                hotfix_pr: None,
                hours_to_hotfix: None,
            })
            .await
            .map_err(|e| format!("append pr.outcome failed: {e}"))?;
    }
    Ok(outcome)
}

/// True when `commit_subject` is a git revert of a commit/PR titled
/// `original_title`. Git's default revert subject is
/// `Revert "<original subject>"`; GitHub's "Revert" button produces
/// `Revert "<PR title> (#<n>)"`. We match the original title as a
/// substring of the quoted portion to tolerate the `(#n)` suffix.
pub fn is_revert_of(commit_subject: &str, original_title: &str) -> bool {
    let subject = commit_subject.trim();
    let Some(rest) = subject.strip_prefix("Revert \"") else {
        return false;
    };
    // Strip the trailing quote (and anything after it, e.g. a PR number).
    let quoted = rest.rsplit_once('"').map(|(q, _)| q).unwrap_or(rest);
    let needle = original_title.trim();
    !needle.is_empty() && quoted.contains(needle)
}

/// Scan a list of commit subjects (newest first, as from
/// `git log --format=%s`) for a revert of any of `pr_titles`. Returns the
/// index of the first matching commit and which title it reverted, or
/// `None` if no revert is found.
pub fn detect_reverts<'a>(
    commit_subjects: &[String],
    pr_titles: &'a [String],
) -> Option<(usize, &'a str)> {
    for (i, subject) in commit_subjects.iter().enumerate() {
        for title in pr_titles {
            if is_revert_of(subject, title) {
                return Some((i, title.as_str()));
            }
        }
    }
    None
}

/// The durable verdict on a merged PR (#183): some days after the merge, did
/// the change hold? Recorded against the models and roles that wrote it, next
/// to the gate pass-rate, which only says the code compiled when the turn
/// ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Later work built on it and it is still mostly there.
    Held,
    /// Reverted, mostly rewritten, broke the base branch, or reopened the
    /// issue it closed.
    Reverted,
    /// No evidence either way. A PR nobody has touched since it merged is not
    /// proof of quality, so this is never counted as a pass.
    Unknown,
}

impl Durability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Reverted => "reverted",
            Self::Unknown => "unknown",
        }
    }
}

/// What the post-merge checks found for one PR. `None` means that check
/// could not be answered (no CI runs, `gh` or `git` unavailable, the merge
/// commit no longer on the base branch).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DurabilityFacts {
    /// A later commit on the base branch reverts the merge.
    pub reverted: Option<bool>,
    /// CI failed on the merge commit and on the next commit after it.
    pub base_stayed_red: Option<bool>,
    /// An issue the PR closed is open again.
    pub issue_reopened: Option<bool>,
    /// Fewer than half the lines the PR added survive on the base branch.
    pub rewritten: Option<bool>,
    /// Later commits on the base branch touched the files the PR changed.
    pub built_on: bool,
}

impl DurabilityFacts {
    /// Any failing signal means `Reverted`. `Held` needs the revert and
    /// rewrite checks answered *and* later work on the same files — surviving
    /// untouched is not surviving anything.
    pub fn verdict(&self) -> Durability {
        let signals = [
            self.reverted,
            self.base_stayed_red,
            self.issue_reopened,
            self.rewritten,
        ];
        if signals.contains(&Some(true)) {
            Durability::Reverted
        } else if self.reverted == Some(false) && self.rewritten == Some(false) && self.built_on {
            Durability::Held
        } else {
            Durability::Unknown
        }
    }
}

/// Ask `gh` and `git` what became of a PR. `Ok(None)` while the PR is not
/// merged, or merged less than `min_age` ago — it is judged once, when old
/// enough. `Err` only when the PR itself cannot be read; a later check that
/// fails just leaves its fact unanswered.
pub fn durability_facts(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    pr_ref: &str,
    min_age: chrono::Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<DurabilityFacts>, String> {
    let body = gh_stdout(
        runner,
        repo_root,
        &[
            "pr",
            "view",
            pr_ref,
            "--json",
            "state,mergedAt,mergeCommit,baseRefName,title,commits,files,closingIssuesReferences",
        ],
    )?;
    if parse_pr_view_json(&body)? != PrState::Merged {
        return Ok(None);
    }
    let pr: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid gh json: {e}"))?;
    let merged_at = pr["mergedAt"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .ok_or("merged PR has no readable mergedAt")?;
    if now.signed_duration_since(merged_at) < min_age {
        return Ok(None);
    }
    let merge_sha = pr["mergeCommit"]["oid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("merged PR has no merge commit")?;
    let base = pr["baseRefName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("merged PR has no base branch")?;
    let title = pr["title"].as_str().unwrap_or("");
    let files = pr["files"].as_array().cloned().unwrap_or_default();
    let paths: Vec<&str> = files.iter().filter_map(|f| f["path"].as_str()).collect();

    let git = |args: &[&str]| {
        runner
            .run("git", args, repo_root)
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout)
    };
    let mut facts = DurabilityFacts::default();

    // Judge against the remote's base branch, not a local one that may be
    // days behind. A failed fetch leaves whatever `origin/<base>` already is.
    let _ = git(&["fetch", "--quiet", "origin", base]);
    let base_ref = format!("origin/{base}");
    // Everything below reads history after the merge, which only means
    // something while the merge commit is still on the base branch.
    if git(&["merge-base", "--is-ancestor", merge_sha, &base_ref]).is_some() {
        let range = format!("{merge_sha}..{base_ref}");
        let revert_body = format!("This reverts commit {merge_sha}");
        facts.reverted = git(&["log", "--format=%s%x1f%b%x1e", &range]).map(|log| {
            log.split('\u{1e}').any(|commit| {
                let commit = commit.trim_start();
                let (subject, body) = commit.split_once('\u{1f}').unwrap_or((commit, ""));
                body.contains(&revert_body) || is_revert_of(subject, title)
            })
        });

        if !paths.is_empty() {
            let mut args = vec!["log", "--format=%H", range.as_str(), "--"];
            args.extend(&paths);
            facts.built_on = git(&args).is_some_and(|out| !out.trim().is_empty());
        }

        // Lines the PR added that `git blame` still attributes to it. A squash
        // merge is one commit (the merge commit); a merge commit keeps the
        // PR's own. ponytail: a rebase merge re-hashes every commit but the
        // last, so its surviving lines undercount; pilot merges squash. A file
        // a later commit renamed reads as deleted, and so as rewritten.
        let added: u64 = files.iter().filter_map(|f| f["additions"].as_u64()).sum();
        let mut ours: std::collections::HashSet<&str> = pr["commits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["oid"].as_str())
            .collect();
        ours.insert(merge_sha);
        let mut surviving = Some(0u64);
        for path in &paths {
            match git(&["blame", "--line-porcelain", &base_ref, "--", path]) {
                Some(out) => {
                    if let Some(n) = surviving.as_mut() {
                        *n += out
                            .lines()
                            .filter(|l| l.split(' ').next().is_some_and(|sha| ours.contains(sha)))
                            .count() as u64;
                    }
                }
                // Blame failing on a file that is still there is a question
                // left unanswered, not proof the lines are gone.
                None if git(&["cat-file", "-e", &format!("{base_ref}:{path}")]).is_some() => {
                    surviving = None;
                }
                // A file gone from the base branch keeps none of its lines.
                None => {}
            }
        }
        facts.rewritten = surviving.map(|n| n * 2 < added);

        let ci_red = |sha: &str| -> Option<bool> {
            let body = gh_stdout(
                runner,
                repo_root,
                &["run", "list", "--commit", sha, "--json", "conclusion"],
            )
            .ok()?;
            let runs: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
            (!runs.is_empty()).then(|| {
                runs.iter().any(|r| {
                    matches!(
                        r["conclusion"].as_str(),
                        Some("failure" | "timed_out" | "startup_failure")
                    )
                })
            })
        };
        facts.base_stayed_red = match ci_red(merge_sha) {
            // Red at landing is only a verdict if the next commit did not fix it.
            Some(true) => git(&["rev-list", "--reverse", "--first-parent", &range])
                .and_then(|out| out.lines().next().map(str::to_string))
                .and_then(|next| ci_red(&next)),
            other => other,
        };
    }

    facts.issue_reopened = pr["closingIssuesReferences"].as_array().and_then(|issues| {
        let mut answered = true;
        for issue in issues {
            let target = issue["url"]
                .as_str()
                .map(str::to_string)
                .or_else(|| issue["number"].as_u64().map(|n| n.to_string()));
            let state = target
                .and_then(|t| {
                    gh_stdout(runner, repo_root, &["issue", "view", &t, "--json", "state"]).ok()
                })
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok());
            match state {
                Some(v)
                    if v["state"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("open")) =>
                {
                    return Some(true);
                }
                Some(_) => {}
                None => answered = false,
            }
        }
        answered.then_some(false)
    });

    Ok(Some(facts))
}

/// Run `gh` and return its stdout, or say which call failed and why.
fn gh_stdout(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    args: &[&str],
) -> Result<String, String> {
    let call = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
    let out = runner
        .run("gh", args, repo_root)
        .map_err(|e| format!("gh {call} failed: {e}"))?;
    if !out.success() {
        return Err(format!("gh {call} exited non-zero: {}", out.stderr.trim()));
    }
    Ok(out.stdout)
}

/// Weighted post-merge stats for one bucket (a run, a role, or a
/// `(role, model)` tuple — the key is the caller's concern). Folds a
/// stream of [`PrOutcomeKind`] into a single adjusted score and the raw
/// counts behind it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeightedStats {
    pub merged: u32,
    pub reverted: u32,
    pub hotfix_followed: u32,
    pub closed: u32,
}

impl WeightedStats {
    pub fn record(&mut self, kind: PrOutcomeKind) {
        match kind {
            PrOutcomeKind::Merged => self.merged += 1,
            PrOutcomeKind::Reverted => self.reverted += 1,
            PrOutcomeKind::HotfixFollowed => self.hotfix_followed += 1,
            PrOutcomeKind::Closed => self.closed += 1,
        }
    }

    pub fn from_outcomes(outcomes: impl IntoIterator<Item = PrOutcomeKind>) -> Self {
        let mut s = Self::default();
        for k in outcomes {
            s.record(k);
        }
        s
    }

    /// Total observed PRs in this bucket.
    pub fn total(&self) -> u32 {
        self.merged + self.reverted + self.hotfix_followed + self.closed
    }

    /// Sum of per-outcome weights (merged +1, reverted −5,
    /// hotfix-followed −2, closed −1). Can be negative.
    pub fn weighted_score(&self) -> f64 {
        self.merged as f64 * PrOutcomeKind::Merged.weight()
            + self.reverted as f64 * PrOutcomeKind::Reverted.weight()
            + self.hotfix_followed as f64 * PrOutcomeKind::HotfixFollowed.weight()
            + self.closed as f64 * PrOutcomeKind::Closed.weight()
    }

    /// Adjusted success rate in `[0.0, 1.0]`, normalised so a bucket of
    /// all-merges scores 1.0 and a bucket of all-reverts scores 0.0.
    ///
    /// The raw weighted score ranges per-PR over `[-5, +1]`; we map that
    /// 6-wide window onto `[0, 1]`. Empty buckets return `None` so the
    /// router can fall back to a prior instead of treating "no data" as
    /// "perfect".
    pub fn adjusted_success_rate(&self) -> Option<f64> {
        let n = self.total();
        if n == 0 {
            return None;
        }
        let best = n as f64 * PrOutcomeKind::Merged.weight(); // +1 each
        let worst = n as f64 * PrOutcomeKind::Reverted.weight(); // −5 each
        let span = best - worst; // always > 0
        Some(((self.weighted_score() - worst) / span).clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_match_plan() {
        assert_eq!(PrOutcomeKind::Merged.weight(), 1.0);
        assert_eq!(PrOutcomeKind::Reverted.weight(), -5.0);
        assert_eq!(PrOutcomeKind::HotfixFollowed.weight(), -2.0);
        assert_eq!(PrOutcomeKind::Closed.weight(), -1.0);
    }

    #[test]
    fn classify_open_pr_is_inconclusive() {
        assert_eq!(classify_pr(PrState::Open, false, false), None);
    }

    #[test]
    fn classify_merged_clean_is_merged() {
        assert_eq!(
            classify_pr(PrState::Merged, false, false),
            Some(PrOutcomeKind::Merged)
        );
    }

    #[test]
    fn classify_revert_beats_hotfix() {
        // A reverted-and-hotfixed PR is recorded as reverted (the harsher
        // signal) so the router penalises it maximally.
        assert_eq!(
            classify_pr(PrState::Merged, true, true),
            Some(PrOutcomeKind::Reverted)
        );
    }

    #[test]
    fn classify_closed_unmerged() {
        assert_eq!(
            classify_pr(PrState::Closed, false, false),
            Some(PrOutcomeKind::Closed)
        );
    }

    #[test]
    fn parse_gh_state_variants() {
        assert_eq!(
            parse_pr_view_json(r#"{"state":"OPEN"}"#).unwrap(),
            PrState::Open
        );
        assert_eq!(
            parse_pr_view_json(r#"{"state":"MERGED"}"#).unwrap(),
            PrState::Merged
        );
        assert_eq!(
            parse_pr_view_json(r#"{"state":"CLOSED"}"#).unwrap(),
            PrState::Closed
        );
    }

    #[test]
    fn parse_gh_falls_back_to_merged_at() {
        let s = parse_pr_view_json(r#"{"mergedAt":"2026-05-01T10:00:00Z"}"#).unwrap();
        assert_eq!(s, PrState::Merged);
    }

    #[test]
    fn parse_gh_rejects_garbage() {
        assert!(parse_pr_view_json("not json").is_err());
    }

    #[test]
    fn is_revert_of_matches_git_default() {
        assert!(is_revert_of(
            r#"Revert "add dark-mode toggle""#,
            "add dark-mode toggle"
        ));
    }

    #[test]
    fn is_revert_of_matches_github_button_with_pr_number() {
        assert!(is_revert_of(
            r#"Revert "add dark-mode toggle (#42)""#,
            "add dark-mode toggle"
        ));
    }

    #[test]
    fn is_revert_of_rejects_non_reverts() {
        assert!(!is_revert_of(
            "add dark-mode toggle",
            "add dark-mode toggle"
        ));
        assert!(!is_revert_of(
            r#"Revert "something else""#,
            "add dark-mode toggle"
        ));
    }

    #[test]
    fn is_revert_of_rejects_empty_title() {
        assert!(!is_revert_of(r#"Revert """#, ""));
    }

    #[test]
    fn detect_reverts_finds_first_match() {
        let log = vec![
            "unrelated commit".to_string(),
            r#"Revert "fix the parser (#7)""#.to_string(),
            "fix the parser".to_string(),
        ];
        let titles = vec!["fix the parser".to_string()];
        let hit = detect_reverts(&log, &titles);
        assert_eq!(hit, Some((1, "fix the parser")));
    }

    #[test]
    fn detect_reverts_returns_none_when_absent() {
        let log = vec!["a".to_string(), "b".to_string()];
        let titles = vec!["c".to_string()];
        assert_eq!(detect_reverts(&log, &titles), None);
    }

    #[test]
    fn weighted_score_sums_per_outcome() {
        let stats = WeightedStats::from_outcomes([
            PrOutcomeKind::Merged,
            PrOutcomeKind::Merged,
            PrOutcomeKind::Reverted,
        ]);
        // 1 + 1 - 5 = -3
        assert_eq!(stats.weighted_score(), -3.0);
        assert_eq!(stats.total(), 3);
    }

    #[test]
    fn adjusted_rate_is_one_for_all_merges() {
        let stats = WeightedStats::from_outcomes([PrOutcomeKind::Merged; 4]);
        assert_eq!(stats.adjusted_success_rate(), Some(1.0));
    }

    #[test]
    fn adjusted_rate_is_zero_for_all_reverts() {
        let stats = WeightedStats::from_outcomes([PrOutcomeKind::Reverted; 3]);
        assert_eq!(stats.adjusted_success_rate(), Some(0.0));
    }

    #[test]
    fn adjusted_rate_none_for_empty_bucket() {
        assert_eq!(WeightedStats::default().adjusted_success_rate(), None);
    }

    use crate::pr::{CommandOut, CommandRunner};
    use std::path::Path as StdPath;

    struct FakeGh {
        state: &'static str,
    }
    impl CommandRunner for FakeGh {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            let stdout = if program == "gh" && args.first().copied() == Some("pr") {
                format!(r#"{{"state":"{}"}}"#, self.state)
            } else {
                String::new()
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn poll_pr_outcome_maps_merged() {
        let r = FakeGh { state: "MERGED" };
        assert_eq!(
            poll_pr_outcome(&r, StdPath::new("."), "42").unwrap(),
            Some(PrOutcomeKind::Merged)
        );
    }

    #[test]
    fn poll_pr_outcome_open_is_none() {
        let r = FakeGh { state: "OPEN" };
        assert_eq!(poll_pr_outcome(&r, StdPath::new("."), "42").unwrap(), None);
    }

    #[test]
    fn poll_pr_outcome_closed() {
        let r = FakeGh { state: "CLOSED" };
        assert_eq!(
            poll_pr_outcome(&r, StdPath::new("."), "42").unwrap(),
            Some(PrOutcomeKind::Closed)
        );
    }

    #[tokio::test]
    async fn poll_and_record_appends_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::RunStore::create(dir.path(), "r1", "g", "abc", "wingman/auto/r1")
                .await
                .unwrap();
        let r = FakeGh { state: "MERGED" };
        let outcome = poll_and_record(&r, &mut store, StdPath::new("."), "42")
            .await
            .unwrap();
        assert_eq!(outcome, Some(PrOutcomeKind::Merged));
        // The event landed in the log.
        let events = store.read_events().await.unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            crate::model::Event::PrOutcome {
                kind: PrOutcomeKind::Merged,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn poll_and_record_open_pr_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::RunStore::create(dir.path(), "r1", "g", "abc", "b")
            .await
            .unwrap();
        let before = store.read_events().await.unwrap().len();
        let r = FakeGh { state: "OPEN" };
        let outcome = poll_and_record(&r, &mut store, StdPath::new("."), "42")
            .await
            .unwrap();
        assert_eq!(outcome, None);
        assert_eq!(store.read_events().await.unwrap().len(), before);
    }

    /// A repository's post-merge history as `gh` and `git` would report it.
    /// Commands not described here fail, the way a missing tool would.
    struct History {
        state: &'static str,
        merged_days_ago: i64,
        on_base: bool,
        /// `git log --format=%s%x1f%b%x1e` over merge..base.
        log: &'static str,
        /// Commits after the merge that touched the PR's files.
        touched: &'static str,
        /// Lines of `a.rs` blame still attributes to the merge commit, of 10 added.
        surviving: usize,
        /// Whether `git blame` runs, and whether `a.rs` is still on the base.
        blame_ok: bool,
        file_on_base: bool,
        /// `gh run list --commit` per sha.
        ci: Vec<(&'static str, &'static str)>,
        issue_state: Option<&'static str>,
    }

    impl Default for History {
        fn default() -> Self {
            Self {
                state: "MERGED",
                merged_days_ago: 40,
                on_base: true,
                log: "",
                touched: "c1\n",
                surviving: 8,
                blame_ok: true,
                file_on_base: true,
                ci: vec![("m1", r#"[{"conclusion":"success"}]"#)],
                issue_state: Some("CLOSED"),
            }
        }
    }

    impl CommandRunner for History {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            let ok = |stdout: String| CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            };
            let fail = CommandOut {
                status: Some(1),
                stdout: String::new(),
                stderr: "no".into(),
            };
            let now = chrono::Utc::now() - chrono::Duration::days(self.merged_days_ago);
            let out = match (program, args) {
                ("gh", ["pr", "view", ..]) => ok(serde_json::json!({
                    "state": self.state,
                    "mergedAt": now.to_rfc3339(),
                    "mergeCommit": {"oid": "m1"},
                    "baseRefName": "main",
                    "title": "add the parser",
                    "commits": [{"oid": "p1"}],
                    "files": [{"path": "a.rs", "additions": 10}],
                    "closingIssuesReferences": match self.issue_state {
                        Some(_) => serde_json::json!([{"number": 7}]),
                        None => serde_json::json!([]),
                    },
                })
                .to_string()),
                ("gh", ["run", "list", "--commit", sha, ..]) => {
                    match self.ci.iter().find(|(s, _)| s == sha) {
                        Some((_, runs)) => ok(runs.to_string()),
                        None => ok("[]".into()),
                    }
                }
                ("gh", ["issue", "view", "7", ..]) => ok(format!(
                    r#"{{"state":"{}"}}"#,
                    self.issue_state.unwrap_or("")
                )),
                ("git", ["fetch", ..]) => ok(String::new()),
                ("git", ["merge-base", ..]) if self.on_base => ok(String::new()),
                ("git", ["log", "--format=%H", ..]) => ok(self.touched.into()),
                ("git", ["log", ..]) => ok(self.log.into()),
                ("git", ["rev-list", ..]) => ok("c1\nc2\n".into()),
                ("git", ["cat-file", ..]) if self.file_on_base => ok(String::new()),
                ("git", ["blame", ..]) if self.blame_ok && self.file_on_base => {
                    let mut out = String::new();
                    for i in 0..10 {
                        let sha = if i < self.surviving { "m1" } else { "c1" };
                        out.push_str(&format!("{sha} {i} {i} 1\nauthor x\n\tline\n"));
                    }
                    ok(out)
                }
                _ => fail,
            };
            Ok(out)
        }
    }

    fn judge(h: &History) -> Option<Durability> {
        durability_facts(
            h,
            StdPath::new("."),
            "https://github.com/o/r/pull/1",
            chrono::Duration::days(30),
            chrono::Utc::now(),
        )
        .unwrap()
        .map(|f| f.verdict())
    }

    #[test]
    fn built_on_and_intact_holds() {
        assert_eq!(judge(&History::default()), Some(Durability::Held));
    }

    #[test]
    fn untouched_since_merge_is_unknown_not_held() {
        let h = History {
            touched: "",
            ..Default::default()
        };
        assert_eq!(judge(&h), Some(Durability::Unknown));
    }

    #[test]
    fn open_or_young_prs_are_not_judged_yet() {
        let open = History {
            state: "OPEN",
            ..Default::default()
        };
        assert_eq!(judge(&open), None);
        let young = History {
            merged_days_ago: 3,
            ..Default::default()
        };
        assert_eq!(judge(&young), None);
    }

    #[test]
    fn a_revert_commit_is_caught_by_body_or_subject() {
        let by_body = History {
            log: "Revert something\u{1f}This reverts commit m1.\n\u{1e}",
            ..Default::default()
        };
        assert_eq!(judge(&by_body), Some(Durability::Reverted));
        let by_subject = History {
            log: "unrelated\u{1f}\u{1e}\nRevert \"add the parser (#1)\"\u{1f}\u{1e}",
            ..Default::default()
        };
        assert_eq!(judge(&by_subject), Some(Durability::Reverted));
    }

    #[test]
    fn most_lines_rewritten_counts_against_it() {
        let h = History {
            surviving: 4,
            ..Default::default()
        };
        assert_eq!(judge(&h), Some(Durability::Reverted));
        // A file deleted since keeps none of its lines.
        let deleted = History {
            file_on_base: false,
            ..Default::default()
        };
        assert_eq!(judge(&deleted), Some(Durability::Reverted));
    }

    #[test]
    fn a_blame_that_fails_on_a_present_file_is_unanswered_not_rewritten() {
        let h = History {
            blame_ok: false,
            ..Default::default()
        };
        assert_eq!(judge(&h), Some(Durability::Unknown));
    }

    #[test]
    fn red_base_counts_only_when_the_next_commit_is_red_too() {
        let fixed = History {
            ci: vec![
                ("m1", r#"[{"conclusion":"failure"}]"#),
                ("c1", r#"[{"conclusion":"success"}]"#),
            ],
            ..Default::default()
        };
        assert_eq!(judge(&fixed), Some(Durability::Held));
        let stayed = History {
            ci: vec![
                (
                    "m1",
                    r#"[{"conclusion":"success"},{"conclusion":"failure"}]"#,
                ),
                ("c1", r#"[{"conclusion":"timed_out"}]"#),
            ],
            ..Default::default()
        };
        assert_eq!(judge(&stayed), Some(Durability::Reverted));
    }

    #[test]
    fn a_reopened_issue_counts_against_it() {
        let h = History {
            issue_state: Some("OPEN"),
            ..Default::default()
        };
        assert_eq!(judge(&h), Some(Durability::Reverted));
    }

    #[test]
    fn history_that_cannot_be_read_is_unknown() {
        // The merge commit is not on the base branch (force-push, or no remote).
        let h = History {
            on_base: false,
            ..Default::default()
        };
        let facts = durability_facts(
            &h,
            StdPath::new("."),
            "1",
            chrono::Duration::days(30),
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(facts.reverted, None);
        assert_eq!(facts.rewritten, None);
        assert_eq!(facts.issue_reopened, Some(false));
        assert_eq!(facts.verdict(), Durability::Unknown);
    }

    #[test]
    fn an_unreadable_pr_is_an_error() {
        struct NoGh;
        impl CommandRunner for NoGh {
            fn run(&self, _: &str, _: &[&str], _: &StdPath) -> std::io::Result<CommandOut> {
                Err(std::io::Error::other("gh not installed"))
            }
        }
        let err = durability_facts(
            &NoGh,
            StdPath::new("."),
            "1",
            chrono::Duration::days(30),
            chrono::Utc::now(),
        )
        .unwrap_err();
        assert!(err.contains("gh pr view"), "{err}");
    }

    #[test]
    fn adjusted_rate_orders_buckets_sensibly() {
        let clean = WeightedStats::from_outcomes([PrOutcomeKind::Merged, PrOutcomeKind::Merged]);
        let mixed =
            WeightedStats::from_outcomes([PrOutcomeKind::Merged, PrOutcomeKind::HotfixFollowed]);
        let bad = WeightedStats::from_outcomes([PrOutcomeKind::Merged, PrOutcomeKind::Reverted]);
        let cr = clean.adjusted_success_rate().unwrap();
        let mr = mixed.adjusted_success_rate().unwrap();
        let br = bad.adjusted_success_rate().unwrap();
        assert!(cr > mr, "clean {cr} should beat mixed {mr}");
        assert!(mr > br, "mixed {mr} should beat reverted {br}");
    }
}
