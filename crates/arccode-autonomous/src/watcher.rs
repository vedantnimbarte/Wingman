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
}
