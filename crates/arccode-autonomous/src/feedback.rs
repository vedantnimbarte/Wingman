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
            crate::store::RunStore::create(dir.path(), "r1", "g", "abc", "arccode/auto/r1")
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
