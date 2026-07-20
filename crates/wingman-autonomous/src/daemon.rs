//! J2 — autonomous goal discovery (daemon scoring core).
//!
//! The daemon polls GitHub issues, failing CI, dependabot PRs, recent
//! TODO/FIXME comments, coverage gaps, and stale deps. For each candidate
//! it computes a `value × confidence ÷ risk` score and decides whether to
//! auto-run, propose, or ignore. The polling/adapters are I/O (and need
//! tokens the plan defers to the user); this module is the scoring +
//! decision core that's testable today.

use std::path::Path;

use crate::intake::TrustLevel;
use crate::pr::CommandRunner;

/// A discovered unit of potential work.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Discovery source label (e.g. "github_issues", "ci_failures").
    pub source: String,
    pub title: String,
    /// Estimated value of doing it, in `[0,1]`.
    pub value: f64,
    /// Confidence the agent can do it correctly, in `[0,1]`.
    pub confidence: f64,
    /// Risk if it goes wrong, in `(0,1]` (higher = riskier).
    pub risk: f64,
    /// Trust of the source channel/author (J3).
    pub trust: TrustLevel,
}

impl Candidate {
    /// `value × confidence ÷ risk`, with risk floored so a near-zero risk
    /// can't produce an infinite score.
    pub fn score(&self) -> f64 {
        let risk = self.risk.max(0.01);
        (self.value.clamp(0.0, 1.0) * self.confidence.clamp(0.0, 1.0)) / risk
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    /// Score clears the auto threshold *and* the source is trusted — run
    /// it without asking.
    AutoRun,
    /// Worth surfacing for a human 👍 before running.
    Propose,
    /// Below the propose floor — log and drop.
    Ignore,
}

/// Decide what to do with a candidate.
///
/// - `AutoRun` requires `score >= auto_threshold` **and**
///   `trust == Trusted` — untrusted work never auto-runs regardless of
///   score (the J15 framing: trust is built by visibly *not* crossing
///   that line).
/// - `Propose` when `score >= propose_floor`.
/// - `Ignore` otherwise.
pub fn decide(candidate: &Candidate, auto_threshold: f64, propose_floor: f64) -> DaemonAction {
    let score = candidate.score();
    if score >= auto_threshold && candidate.trust == TrustLevel::Trusted {
        DaemonAction::AutoRun
    } else if score >= propose_floor {
        DaemonAction::Propose
    } else {
        DaemonAction::Ignore
    }
}

/// J2 discovery shell: fetch open issues carrying `label` via
/// `gh issue list` and map each to a [`Candidate`]. The polling cadence
/// and the long-running daemon loop are the runtime's concern; this is the
/// one-shot fetch+map, testable with a mock runner. Author trust is
/// resolved against `trusted_authors`.
pub fn fetch_issue_candidates(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    label: &str,
    trusted_authors: &[String],
) -> Result<Vec<Candidate>, String> {
    let out = runner
        .run(
            "gh",
            &[
                "issue",
                "list",
                "--label",
                label,
                "--json",
                "number,title,author",
            ],
            repo_root,
        )
        .map_err(|e| format!("gh issue list failed: {e}"))?;
    if !out.success() {
        return Err(format!(
            "gh issue list exited non-zero: {}",
            out.stderr.trim()
        ));
    }
    let items: serde_json::Value =
        serde_json::from_str(&out.stdout).map_err(|e| format!("bad gh json: {e}"))?;
    let arr = items.as_array().cloned().unwrap_or_default();
    let mut candidates = Vec::new();
    for it in arr {
        let number = it.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let title = it
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("(untitled)")
            .to_string();
        let author = it
            .pointer("/author/login")
            .and_then(|a| a.as_str())
            .map(|s| s.to_string());
        let trust = match author {
            Some(ref a) if trusted_authors.iter().any(|t| t.eq_ignore_ascii_case(a)) => {
                TrustLevel::Trusted
            }
            Some(_) => TrustLevel::Known,
            None => TrustLevel::Untrusted,
        };
        candidates.push(Candidate {
            source: format!("github_issue#{number}"),
            title,
            // Neutral priors until a richer scorer (J9) is plumbed in.
            value: 0.6,
            confidence: 0.6,
            risk: 0.4,
            trust,
        });
    }
    Ok(candidates)
}

/// Cap on how many TODO/FIXME markers one poll surfaces, so a repo with
/// hundreds of them doesn't flood the queue in a single cycle.
const MAX_TODO_CANDIDATES: usize = 20;

/// Local discovery source: scan the working tree for `TODO`/`FIXME`
/// markers via `git grep` (respects `.gitignore`, no network) and map each
/// to a [`Candidate`]. Unlike `github_issues` these carry no external
/// author, so trust is [`TrustLevel::Known`] — they surface as proposals
/// but never auto-run.
pub fn fetch_todo_candidates(
    runner: &dyn CommandRunner,
    repo_root: &Path,
) -> Result<Vec<Candidate>, String> {
    let out = runner
        .run("git", &["grep", "-n", "-I", "-E", "TODO|FIXME"], repo_root)
        .map_err(|e| format!("git grep failed: {e}"))?;
    // git grep exits 1 with no matches — that's not an error, just no work.
    if !out.success() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for line in out.stdout.lines() {
        // Format: `path:linenum:content`.
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(_lineno), Some(content)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let title = content.trim();
        let title = title.strip_prefix("//").unwrap_or(title).trim();
        candidates.push(Candidate {
            source: format!("todo:{path}"),
            title: title.to_string(),
            // A loose marker: modest value, medium confidence, low risk.
            value: 0.4,
            confidence: 0.5,
            risk: 0.3,
            trust: TrustLevel::Known,
        });
        if candidates.len() >= MAX_TODO_CANDIDATES {
            break;
        }
    }
    Ok(candidates)
}

/// Cap on how many candidates each `gh`-backed source surfaces per poll.
const MAX_GH_CANDIDATES: usize = 20;

/// J2 discovery source: failed CI runs via `gh run list --status failure`.
/// Each failing workflow run maps to a "fix the failing build" candidate.
/// These are repo-internal (no external author) so trust is [`TrustLevel::Known`]
/// — they surface as proposals, high value (a red build blocks everyone).
pub fn fetch_ci_failure_candidates(
    runner: &dyn CommandRunner,
    repo_root: &Path,
) -> Result<Vec<Candidate>, String> {
    let out = runner
        .run(
            "gh",
            &[
                "run",
                "list",
                "--status",
                "failure",
                "--limit",
                "20",
                "--json",
                "databaseId,displayTitle,workflowName",
            ],
            repo_root,
        )
        .map_err(|e| format!("gh run list failed: {e}"))?;
    if !out.success() {
        return Err(format!(
            "gh run list exited non-zero: {}",
            out.stderr.trim()
        ));
    }
    let items: serde_json::Value =
        serde_json::from_str(&out.stdout).map_err(|e| format!("bad gh json: {e}"))?;
    let mut candidates = Vec::new();
    for it in items.as_array().cloned().unwrap_or_default() {
        let id = it.get("databaseId").and_then(|n| n.as_u64()).unwrap_or(0);
        let workflow = it
            .get("workflowName")
            .and_then(|w| w.as_str())
            .unwrap_or("CI");
        let run_title = it
            .get("displayTitle")
            .and_then(|t| t.as_str())
            .unwrap_or("(untitled run)");
        candidates.push(Candidate {
            source: format!("ci_failure#{id}"),
            title: format!("Fix failing {workflow}: {run_title}"),
            // A red build is high-value, moderate-confidence (root cause
            // varies), moderate-risk work.
            value: 0.8,
            confidence: 0.5,
            risk: 0.5,
            trust: TrustLevel::Known,
        });
        if candidates.len() >= MAX_GH_CANDIDATES {
            break;
        }
    }
    Ok(candidates)
}

/// J2 discovery source: open Dependabot PRs via `gh pr list`. Each is a
/// dependency bump to review/merge. Dependabot is a known bot; a bump is
/// low-risk and high-confidence, so these surface as proposals (auto-run
/// still requires the trust config to clear the threshold).
pub fn fetch_dependabot_candidates(
    runner: &dyn CommandRunner,
    repo_root: &Path,
) -> Result<Vec<Candidate>, String> {
    let out = runner
        .run(
            "gh",
            &[
                "pr",
                "list",
                "--author",
                "app/dependabot",
                "--json",
                "number,title",
            ],
            repo_root,
        )
        .map_err(|e| format!("gh pr list failed: {e}"))?;
    if !out.success() {
        return Err(format!("gh pr list exited non-zero: {}", out.stderr.trim()));
    }
    let items: serde_json::Value =
        serde_json::from_str(&out.stdout).map_err(|e| format!("bad gh json: {e}"))?;
    let mut candidates = Vec::new();
    for it in items.as_array().cloned().unwrap_or_default() {
        let number = it.get("number").and_then(|n| n.as_u64()).unwrap_or(0);
        let title = it
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("(untitled)");
        candidates.push(Candidate {
            source: format!("dependabot#{number}"),
            title: format!("Dependency update: {title}"),
            value: 0.5,
            confidence: 0.8,
            risk: 0.3,
            trust: TrustLevel::Known,
        });
        if candidates.len() >= MAX_GH_CANDIDATES {
            break;
        }
    }
    Ok(candidates)
}

/// J2 discovery source: under-tested files from an existing lcov report
/// (`lcov.info` / `coverage.lcov` / `coverage/lcov.info`, whichever exists).
/// Reads coverage the user already generated — it does not run a coverage
/// tool. Files below `min_ratio` line coverage become "add tests" candidates.
pub fn fetch_coverage_gap_candidates(
    repo_root: &Path,
    min_ratio: f64,
) -> Result<Vec<Candidate>, String> {
    let lcov = ["lcov.info", "coverage.lcov", "coverage/lcov.info"]
        .iter()
        .map(|p| repo_root.join(p))
        .find(|p| p.exists());
    let Some(path) = lcov else {
        return Ok(Vec::new()); // no coverage report → nothing to surface
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {path:?}: {e}"))?;
    let mut candidates = Vec::new();
    let mut file: Option<String> = None;
    let (mut found, mut hit) = (0u64, 0u64);
    for line in text.lines() {
        if let Some(sf) = line.strip_prefix("SF:") {
            file = Some(sf.trim().to_string());
            found = 0;
            hit = 0;
        } else if let Some(lf) = line.strip_prefix("LF:") {
            found = lf.trim().parse().unwrap_or(0);
        } else if let Some(lh) = line.strip_prefix("LH:") {
            hit = lh.trim().parse().unwrap_or(0);
        } else if line.starts_with("end_of_record") {
            if let Some(f) = file.take() {
                let ratio = if found == 0 {
                    1.0
                } else {
                    hit as f64 / found as f64
                };
                if found > 0 && ratio < min_ratio {
                    let pct = (ratio * 100.0).round() as u64;
                    candidates.push(Candidate {
                        source: format!("coverage:{f}"),
                        title: format!("Add tests for {f} ({pct}% line coverage)"),
                        value: 0.4,
                        confidence: 0.6,
                        risk: 0.2,
                        trust: TrustLevel::Known,
                    });
                }
            }
            found = 0;
            hit = 0;
        }
        if candidates.len() >= MAX_GH_CANDIDATES {
            break;
        }
    }
    Ok(candidates)
}

/// J3 file-drop intake: each `*.md` in `dir` is a person's request (a
/// Slack/email gateway writes them there). Read via [`crate::intake::scan_inbox`]
/// and map to candidates carrying the message's trust — a trusted author's
/// drop can auto-run, an unknown one surfaces as a proposal. Missing dir →
/// empty (not an error).
pub fn fetch_intake_candidates(dir: &Path, trusted_authors: &[String]) -> Vec<Candidate> {
    crate::intake::scan_inbox(dir, trusted_authors)
        .into_iter()
        .take(MAX_GH_CANDIDATES)
        .map(|g| {
            let title: String = g
                .text
                .lines()
                .next()
                .unwrap_or(&g.text)
                .chars()
                .take(120)
                .collect();
            Candidate {
                source: format!("intake:{}", g.source.as_str()),
                title,
                // A human explicitly asked for this: high value.
                value: 0.8,
                confidence: 0.5,
                risk: 0.4,
                trust: g.trust_level,
            }
        })
        .collect()
}

/// J2 discovery cycle: one full pass of the daemon — fetch candidates
/// from the configured sources, rank them, and decide an action for each.
/// The infinite scheduling (sleep `poll_interval_secs`, repeat) is trivial
/// process supervision layered on top; this is the testable unit of work.
///
/// `propose_floor` is the score below which a candidate is ignored
/// entirely (typically `auto_threshold * 0.4`).
pub fn run_cycle(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    cfg: &wingman_config::PilotDaemonConfig,
    propose_floor: f64,
) -> Vec<(Candidate, DaemonAction)> {
    let mut candidates = Vec::new();
    if cfg.sources.iter().any(|s| s == "github_issues") {
        for label in &cfg.trusted_labels {
            if let Ok(found) =
                fetch_issue_candidates(runner, repo_root, label, &cfg.trusted_authors)
            {
                candidates.extend(found);
            }
        }
    }
    if cfg.sources.iter().any(|s| s == "todos") {
        if let Ok(found) = fetch_todo_candidates(runner, repo_root) {
            candidates.extend(found);
        }
    }
    if cfg.sources.iter().any(|s| s == "ci_failures") {
        if let Ok(found) = fetch_ci_failure_candidates(runner, repo_root) {
            candidates.extend(found);
        }
    }
    if cfg.sources.iter().any(|s| s == "dependabot") {
        if let Ok(found) = fetch_dependabot_candidates(runner, repo_root) {
            candidates.extend(found);
        }
    }
    if cfg.sources.iter().any(|s| s == "coverage_gaps") {
        // Surface files below 50% line coverage from an existing lcov report.
        if let Ok(found) = fetch_coverage_gap_candidates(repo_root, 0.5) {
            candidates.extend(found);
        }
    }
    if cfg.sources.iter().any(|s| s == "intake") {
        let dir = repo_root.join(&cfg.intake_dir);
        candidates.extend(fetch_intake_candidates(&dir, &cfg.trusted_authors));
    }
    rank(candidates)
        .into_iter()
        .map(|c| {
            let action = decide(&c, cfg.auto_threshold, propose_floor);
            (c, action)
        })
        .collect()
}

/// Bounded form of the daemon's poll loop: run [`run_cycle`] `cycles`
/// times, invoking `on_actions` with each pass's results. This is the
/// testable core of the long-running daemon; production wraps it as
/// `loop { run_cycle(...); on_actions(...); sleep(poll_interval) }`. We
/// expose the bounded variant so the loop body is unit-tested and the only
/// untested part is the literal `sleep` + non-termination.
pub fn run_n_cycles<F>(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    cfg: &wingman_config::PilotDaemonConfig,
    propose_floor: f64,
    cycles: usize,
    mut on_actions: F,
) where
    F: FnMut(usize, Vec<(Candidate, DaemonAction)>),
{
    for i in 0..cycles {
        let actions = run_cycle(runner, repo_root, cfg, propose_floor);
        on_actions(i, actions);
    }
}

/// Rank candidates best-first by score (ties broken by title for
/// determinism). Useful for the daemon's `queue list`.
pub fn rank(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.title.cmp(&b.title))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(value: f64, confidence: f64, risk: f64, trust: TrustLevel) -> Candidate {
        Candidate {
            source: "github_issues".into(),
            title: "fix #142".into(),
            value,
            confidence,
            risk,
            trust,
        }
    }

    #[test]
    fn score_is_value_times_confidence_over_risk() {
        let c = cand(0.8, 0.9, 0.2, TrustLevel::Trusted);
        assert!((c.score() - (0.8 * 0.9 / 0.2)).abs() < 1e-9);
    }

    #[test]
    fn risk_is_floored() {
        let c = cand(1.0, 1.0, 0.0, TrustLevel::Trusted);
        assert!(c.score().is_finite());
        assert!((c.score() - 100.0).abs() < 1e-9); // 1/0.01
    }

    #[test]
    fn high_score_trusted_auto_runs() {
        let c = cand(0.9, 0.9, 0.5, TrustLevel::Trusted); // score 1.62
        assert_eq!(decide(&c, 0.75, 0.3), DaemonAction::AutoRun);
    }

    #[test]
    fn high_score_untrusted_only_proposes() {
        let c = cand(0.9, 0.9, 0.5, TrustLevel::Known); // score 1.62 but not trusted
        assert_eq!(decide(&c, 0.75, 0.3), DaemonAction::Propose);
    }

    #[test]
    fn untrusted_never_auto_runs_even_at_max_score() {
        let c = cand(1.0, 1.0, 0.01, TrustLevel::Untrusted); // huge score
        assert_eq!(decide(&c, 0.75, 0.3), DaemonAction::Propose);
    }

    #[test]
    fn mid_score_proposes() {
        let c = cand(0.5, 0.5, 0.5, TrustLevel::Trusted); // score 0.5
        assert_eq!(decide(&c, 0.75, 0.3), DaemonAction::Propose);
    }

    #[test]
    fn low_score_ignored() {
        let c = cand(0.2, 0.2, 0.9, TrustLevel::Trusted); // score ~0.044
        assert_eq!(decide(&c, 0.75, 0.3), DaemonAction::Ignore);
    }

    use crate::pr::{CommandOut, CommandRunner};
    use std::path::Path as StdPath;

    struct FakeIssueGh;
    impl CommandRunner for FakeIssueGh {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            let stdout = if program == "gh" && args.first().copied() == Some("issue") {
                r#"[{"number":142,"title":"fix the parser","author":{"login":"vedant"}},
                    {"number":7,"title":"typo","author":{"login":"stranger"}}]"#
                    .to_string()
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

    struct FakeTodoGrep;
    impl CommandRunner for FakeTodoGrep {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            let stdout = if program == "git" && args.first().copied() == Some("grep") {
                "src/a.rs:12:    // TODO: handle empty input\nsrc/b.rs:3:// FIXME broken parse\n"
                    .to_string()
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
    fn fetch_todo_candidates_parses_and_marks_known() {
        let cands = fetch_todo_candidates(&FakeTodoGrep, StdPath::new(".")).unwrap();
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].source, "todo:src/a.rs");
        assert_eq!(cands[0].title, "TODO: handle empty input");
        // Local markers never carry enough trust to auto-run.
        assert!(cands.iter().all(|c| c.trust == TrustLevel::Known));
        assert!(cands
            .iter()
            .all(|c| decide(c, 0.75, 0.3) != DaemonAction::AutoRun));
    }

    struct NoMatchGrep;
    impl CommandRunner for NoMatchGrep {
        fn run(&self, _p: &str, _a: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            // git grep exits 1 with no matches.
            Ok(CommandOut {
                status: Some(1),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn fetch_todo_candidates_empty_on_no_matches() {
        let cands = fetch_todo_candidates(&NoMatchGrep, StdPath::new(".")).unwrap();
        assert!(cands.is_empty());
    }

    #[test]
    fn fetch_issue_candidates_maps_and_trusts() {
        let cands = fetch_issue_candidates(
            &FakeIssueGh,
            StdPath::new("."),
            "wingman:auto",
            &["vedant".to_string()],
        )
        .unwrap();
        assert_eq!(cands.len(), 2);
        let parser = cands.iter().find(|c| c.title == "fix the parser").unwrap();
        assert_eq!(parser.trust, TrustLevel::Trusted);
        let typo = cands.iter().find(|c| c.title == "typo").unwrap();
        assert_eq!(typo.trust, TrustLevel::Known);
    }

    #[test]
    fn run_cycle_fetches_scores_and_decides() {
        let cfg = wingman_config::PilotDaemonConfig {
            sources: vec!["github_issues".into()],
            trusted_labels: vec!["wingman:auto".into()],
            trusted_authors: vec!["vedant".into()],
            auto_threshold: 0.75,
            ..Default::default()
        };
        let results = run_cycle(&FakeIssueGh, StdPath::new("."), &cfg, 0.3);
        assert_eq!(results.len(), 2);
        // Both score 0.6*0.6/0.4 = 0.9 ≥ auto_threshold; the trusted one
        // auto-runs, the unknown one only proposes.
        let parser = results
            .iter()
            .find(|(c, _)| c.title == "fix the parser")
            .unwrap();
        assert_eq!(parser.1, DaemonAction::AutoRun);
        let typo = results.iter().find(|(c, _)| c.title == "typo").unwrap();
        assert_eq!(typo.1, DaemonAction::Propose);
    }

    #[test]
    fn run_n_cycles_invokes_callback_per_pass() {
        let cfg = wingman_config::PilotDaemonConfig {
            sources: vec!["github_issues".into()],
            trusted_labels: vec!["wingman:auto".into()],
            trusted_authors: vec!["vedant".into()],
            auto_threshold: 0.75,
            ..Default::default()
        };
        let mut passes = 0;
        let mut total_actions = 0;
        run_n_cycles(
            &FakeIssueGh,
            StdPath::new("."),
            &cfg,
            0.3,
            3,
            |_, actions| {
                passes += 1;
                total_actions += actions.len();
            },
        );
        assert_eq!(passes, 3);
        assert_eq!(total_actions, 6); // 2 candidates × 3 cycles
    }

    #[test]
    fn rank_orders_by_score_desc() {
        let cands = vec![
            Candidate {
                title: "low".into(),
                ..cand(0.2, 0.2, 0.9, TrustLevel::Trusted)
            },
            Candidate {
                title: "high".into(),
                ..cand(0.9, 0.9, 0.2, TrustLevel::Trusted)
            },
            Candidate {
                title: "mid".into(),
                ..cand(0.5, 0.5, 0.5, TrustLevel::Trusted)
            },
        ];
        let ranked = rank(cands);
        assert_eq!(ranked[0].title, "high");
        assert_eq!(ranked[2].title, "low");
    }

    struct FakeGhJson(&'static str, &'static str); // (subcommand, json)
    impl CommandRunner for FakeGhJson {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            let stdout = if program == "gh" && args.first().copied() == Some(self.0) {
                self.1.to_string()
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
    fn fetch_ci_failures_maps_runs() {
        let gh = FakeGhJson(
            "run",
            r#"[{"databaseId":991,"displayTitle":"Merge #4","workflowName":"ci"}]"#,
        );
        let c = fetch_ci_failure_candidates(&gh, StdPath::new(".")).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].source, "ci_failure#991");
        assert!(c[0].title.contains("Fix failing ci"));
        assert_eq!(c[0].trust, TrustLevel::Known);
    }

    #[test]
    fn fetch_dependabot_maps_prs() {
        let gh = FakeGhJson("pr", r#"[{"number":12,"title":"bump serde to 1.0.2"}]"#);
        let c = fetch_dependabot_candidates(&gh, StdPath::new(".")).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].source, "dependabot#12");
        assert!(c[0].title.contains("bump serde"));
    }

    #[test]
    fn fetch_intake_maps_file_drops_to_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        // A trusted author's drop → Trusted; an unknown one → Known.
        std::fs::write(
            tmp.path().join("a.md"),
            "author: vedant\nAdd a --version flag to the CLI.\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("b.md"), "Fix the flaky test.\n").unwrap();
        let cands = fetch_intake_candidates(tmp.path(), &["vedant".to_string()]);
        assert_eq!(cands.len(), 2);
        assert!(cands.iter().all(|c| c.source.starts_with("intake:")));
        let trusted = cands
            .iter()
            .find(|c| c.title.contains("--version"))
            .unwrap();
        assert_eq!(trusted.trust, TrustLevel::Trusted);
        // Missing dir → empty, not an error.
        assert!(fetch_intake_candidates(StdPath::new("/no/such/dir"), &[]).is_empty());
    }

    #[test]
    fn fetch_coverage_gaps_flags_undertested_files() {
        let tmp = tempfile::tempdir().unwrap();
        // src/low.rs at 25% (< 50%), src/high.rs at 90% (kept).
        std::fs::write(
            tmp.path().join("lcov.info"),
            "SF:src/low.rs\nLF:4\nLH:1\nend_of_record\nSF:src/high.rs\nLF:10\nLH:9\nend_of_record\n",
        )
        .unwrap();
        let c = fetch_coverage_gap_candidates(tmp.path(), 0.5).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].source, "coverage:src/low.rs");
        assert!(c[0].title.contains("25%"));
        // No lcov file → no candidates, not an error.
        let empty = tempfile::tempdir().unwrap();
        assert!(fetch_coverage_gap_candidates(empty.path(), 0.5)
            .unwrap()
            .is_empty());
    }
}
