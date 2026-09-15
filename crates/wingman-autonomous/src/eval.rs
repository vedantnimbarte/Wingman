//! R4 — eval / regression harness (scoring core).
//!
//! A scheduled run of canned goals, each pinned to a base revision, scored on
//! four axes: success rate, cost, wall time, and an LLM-judge quality score.
//! `.github/workflows/eval.yml` runs the suite weekly and fails when any axis
//! regresses more than a threshold (default 10%) against the committed
//! baseline.
//!
//! This module is the scoring + comparison core: the goals-file format, the
//! golden references and the judge that grades a run's diff against one, and
//! the summary + regression check. Running the goals is the CLI's job
//! (`wingman pilot eval`). Axis direction matters: success rate and quality
//! are "higher is better"; cost and wall time are "lower is better".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::planner::PlannerLlm;
use crate::pr::CommandRunner;

/// One canned-goal result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalResult {
    pub goal: String,
    /// Did the run reach Done?
    pub success: bool,
    pub usd: f64,
    pub wall_min: f64,
    /// LLM-judge diff-quality score in `[0,1]` vs a golden diff, or the
    /// success proxy (1.0/0.0) when `judged` is false.
    pub quality: f64,
    /// True when `quality` came from the judge against a golden reference.
    /// Absent in results written before golden references existed.
    #[serde(default)]
    pub judged: bool,
}

/// One canned goal. In a goals file a plain line is a goal with no golden
/// reference; a line starting with `{` is this object as JSON.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalGoal {
    pub goal: String,
    /// Revision the run starts from. Defaults to `golden_commit`'s parent,
    /// so the run faces the tree the golden commit was written against.
    #[serde(default)]
    pub base: Option<String>,
    /// A commit whose own change is the golden reference.
    #[serde(default)]
    pub golden_commit: Option<String>,
    /// A unified diff file holding the golden reference, relative to the
    /// goals file.
    #[serde(default)]
    pub golden_diff: Option<PathBuf>,
}

impl EvalGoal {
    /// The revision to pin the run to, when the goal names one.
    pub fn base(&self) -> Option<String> {
        self.base
            .clone()
            .or_else(|| self.golden_commit.as_ref().map(|c| format!("{c}^")))
    }
}

/// Parse a goals file. Blank lines and `#` comments are skipped. A malformed
/// JSON line is an error naming its line, not a skipped goal: a suite that
/// quietly shrinks would compare against a baseline it no longer matches.
pub fn parse_goals(text: &str) -> Result<Vec<EvalGoal>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with('{') {
            out.push(EvalGoal {
                goal: line.to_string(),
                base: None,
                golden_commit: None,
                golden_diff: None,
            });
            continue;
        }
        let n = i + 1;
        let g: EvalGoal = serde_json::from_str(line).map_err(|e| format!("line {n}: {e}"))?;
        if g.goal.trim().is_empty() {
            return Err(format!("line {n}: empty goal"));
        }
        if g.golden_commit.is_some() && g.golden_diff.is_some() {
            return Err(format!(
                "line {n}: set golden_commit or golden_diff, not both"
            ));
        }
        // Revisions reach git as arguments; one starting with `-` would be
        // read as an option.
        for rev in [&g.base, &g.golden_commit].into_iter().flatten() {
            if rev.is_empty() || rev.starts_with('-') {
                return Err(format!("line {n}: invalid revision {rev:?}"));
            }
        }
        out.push(g);
    }
    Ok(out)
}

/// `git diff <from> <to>` in `repo_root`.
pub fn git_diff(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    from: &str,
    to: &str,
) -> Result<String, String> {
    let out = runner
        .run(
            "git",
            &["diff", "--no-color", "--no-ext-diff", from, to, "--"],
            repo_root,
        )
        .map_err(|e| format!("git diff {from} {to}: {e}"))?;
    if !out.success() {
        return Err(format!("git diff {from} {to}: {}", out.stderr.trim()));
    }
    Ok(out.stdout)
}

/// The goal's golden diff, `None` when it has no golden reference.
pub fn golden_diff(
    runner: &dyn CommandRunner,
    repo_root: &Path,
    goals_dir: &Path,
    goal: &EvalGoal,
) -> Result<Option<String>, String> {
    let diff = if let Some(commit) = &goal.golden_commit {
        git_diff(runner, repo_root, &format!("{commit}^"), commit)?
    } else if let Some(path) = &goal.golden_diff {
        let path = goals_dir.join(path);
        std::fs::read_to_string(&path)
            .map_err(|e| format!("reading golden diff {}: {e}", path.display()))?
    } else {
        return Ok(None);
    };
    if diff.trim().is_empty() {
        return Err("the golden reference is an empty diff".into());
    }
    Ok(Some(diff))
}

/// Characters of each diff the judge is shown.
const JUDGE_DIFF_CHARS: usize = 30_000;

/// The judge's grade of one run's diff.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct JudgeVerdict {
    pub score: f64,
    #[serde(default)]
    pub reason: String,
}

/// Ask the judge how well `candidate` achieves `goal`, with `golden` as a
/// known-good solution. A score outside `[0,1]` is an error rather than
/// clamped: a reply on another scale (7 of 10) would otherwise read as 1.0.
pub async fn judge_diff(
    llm: &dyn PlannerLlm,
    goal: &str,
    golden: &str,
    candidate: &str,
) -> Result<JudgeVerdict, String> {
    const SYSTEM: &str = "You grade a code change against a reference change made for the \
        same goal. The reference is one correct solution, not the only one: judge whether the \
        candidate achieves what the reference achieves (behaviour, tests, scope), not whether \
        its text matches. Take points off for missing behaviour, missing tests the reference \
        added, and broken or unrelated changes. Reply with ONLY a JSON object: \
        {\"score\": <number from 0 to 1>, \"reason\": \"<one sentence>\"}.";
    let user = format!(
        "Goal: {goal}\n\nReference diff:\n{}\n\nCandidate diff:\n{}",
        clip(golden, JUDGE_DIFF_CHARS),
        clip(candidate, JUDGE_DIFF_CHARS)
    );
    let raw = llm
        .complete(SYSTEM.into(), user)
        .await
        .map_err(|e| format!("judge call failed: {e}"))?;
    let json = crate::planner::extract_json_object(&raw)
        .ok_or_else(|| "judge reply had no JSON object".to_string())?;
    let verdict: JudgeVerdict =
        serde_json::from_str(&json).map_err(|e| format!("invalid judge verdict: {e}"))?;
    if !(0.0..=1.0).contains(&verdict.score) {
        return Err(format!("judge score {} is outside [0,1]", verdict.score));
    }
    Ok(verdict)
}

/// The first `max` characters of `s`, marked when cut.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\n[... diff truncated]", &s[..cut]),
        None => s.to_string(),
    }
}

/// How one goal's quality was scored.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityScore {
    pub quality: f64,
    pub judged: bool,
    /// The judge's reason, or why the score fell back to the success proxy.
    pub note: Option<String>,
}

/// Score a goal's quality: the judge's grade of the run's diff (`run_range`
/// is its base commit and integration branch) against the goal's golden
/// reference. Without a golden reference, or for a failed run, it is the
/// success proxy (1.0/0.0). A reference, run or judge that cannot be used
/// also falls back to the proxy, with a note saying why.
pub async fn score_quality(
    judge: Option<&dyn PlannerLlm>,
    runner: &dyn CommandRunner,
    repo_root: &Path,
    goals_dir: &Path,
    goal: &EvalGoal,
    success: bool,
    run_range: Option<(&str, &str)>,
) -> QualityScore {
    let proxy = |note: Option<String>| QualityScore {
        quality: if success { 1.0 } else { 0.0 },
        judged: false,
        note,
    };
    if !success {
        return proxy(None);
    }
    let golden = match golden_diff(runner, repo_root, goals_dir, goal) {
        Ok(Some(golden)) => golden,
        Ok(None) => return proxy(None),
        Err(e) => return proxy(Some(e)),
    };
    let Some(judge) = judge else {
        return proxy(Some("no judge model is available".into()));
    };
    let Some((base, branch)) = run_range else {
        return proxy(Some("the run's record was not found".into()));
    };
    let candidate = match git_diff(runner, repo_root, base, branch) {
        Ok(diff) => diff,
        Err(e) => return proxy(Some(e)),
    };
    if candidate.trim().is_empty() {
        return QualityScore {
            quality: 0.0,
            judged: true,
            note: Some("the run changed nothing".into()),
        };
    }
    match judge_diff(judge, &goal.goal, &golden, &candidate).await {
        Ok(v) => QualityScore {
            quality: v.score,
            judged: true,
            note: Some(v.reason),
        },
        Err(e) => proxy(Some(e)),
    }
}

/// Aggregate across a suite of results.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalSummary {
    pub n: usize,
    pub success_rate: f64,
    pub avg_usd: f64,
    pub avg_wall: f64,
    pub avg_quality: f64,
}

pub fn summarize(results: &[EvalResult]) -> EvalSummary {
    let n = results.len();
    if n == 0 {
        return EvalSummary {
            n: 0,
            success_rate: 0.0,
            avg_usd: 0.0,
            avg_wall: 0.0,
            avg_quality: 0.0,
        };
    }
    let nf = n as f64;
    let successes = results.iter().filter(|r| r.success).count() as f64;
    EvalSummary {
        n,
        success_rate: successes / nf,
        avg_usd: results.iter().map(|r| r.usd).sum::<f64>() / nf,
        avg_wall: results.iter().map(|r| r.wall_min).sum::<f64>() / nf,
        avg_quality: results.iter().map(|r| r.quality).sum::<f64>() / nf,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Higher is better (success rate, quality).
    HigherBetter,
    /// Lower is better (cost, wall time).
    LowerBetter,
}

/// Per-axis comparison vs baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisDelta {
    pub name: String,
    pub baseline: f64,
    pub current: f64,
    /// Signed fractional change `(current - baseline) / baseline`.
    pub pct_change: f64,
    /// True when this axis regressed beyond the threshold.
    pub regressed: bool,
}

/// Full regression report.
#[derive(Debug, Clone, PartialEq)]
pub struct RegressionReport {
    pub axes: Vec<AxisDelta>,
    /// True when any axis regressed.
    pub regressed: bool,
}

fn axis(name: &str, baseline: f64, current: f64, dir: Direction, threshold: f64) -> AxisDelta {
    // pct change relative to baseline; if baseline is 0 we can't form a
    // ratio, so treat any worsening as a flat regression and improvement
    // as fine.
    let pct_change = if baseline == 0.0 {
        if current == 0.0 {
            0.0
        } else {
            // From-zero change is undefined as a ratio; report the raw
            // delta sign via ±infinity-ish sentinel kept finite.
            match dir {
                Direction::HigherBetter => 1.0, // current>0 is an improvement
                Direction::LowerBetter => 1.0,  // current>0 is a worsening
            }
        }
    } else {
        (current - baseline) / baseline
    };

    let regressed = match dir {
        Direction::HigherBetter => {
            // Worse means current dropped below baseline by > threshold.
            baseline > 0.0 && current < baseline * (1.0 - threshold)
        }
        Direction::LowerBetter => {
            if baseline == 0.0 {
                current > 0.0
            } else {
                current > baseline * (1.0 + threshold)
            }
        }
    };

    AxisDelta {
        name: name.to_string(),
        baseline,
        current,
        pct_change,
        regressed,
    }
}

/// Compare a current summary to a baseline. `threshold` is the allowed
/// fractional drift (0.10 = 10%).
pub fn compare(current: &EvalSummary, baseline: &EvalSummary, threshold: f64) -> RegressionReport {
    let axes = vec![
        axis(
            "success_rate",
            baseline.success_rate,
            current.success_rate,
            Direction::HigherBetter,
            threshold,
        ),
        axis(
            "avg_usd",
            baseline.avg_usd,
            current.avg_usd,
            Direction::LowerBetter,
            threshold,
        ),
        axis(
            "avg_wall",
            baseline.avg_wall,
            current.avg_wall,
            Direction::LowerBetter,
            threshold,
        ),
        axis(
            "avg_quality",
            baseline.avg_quality,
            current.avg_quality,
            Direction::HigherBetter,
            threshold,
        ),
    ];
    let regressed = axes.iter().any(|a| a.regressed);
    RegressionReport { axes, regressed }
}

/// Render a markdown dashboard row-set for `.wingman/eval/`.
pub fn render_report(report: &RegressionReport) -> String {
    let mut out = String::from("# Eval regression report\n\n");
    out.push_str(&format!(
        "**Overall: {}**\n\n",
        if report.regressed {
            "⛔ REGRESSED"
        } else {
            "✅ within tolerance"
        }
    ));
    out.push_str("| Axis | Baseline | Current | Δ | Status |\n");
    out.push_str("| ---- | -------- | ------- | - | ------ |\n");
    for a in &report.axes {
        out.push_str(&format!(
            "| {} | {:.3} | {:.3} | {:+.1}% | {} |\n",
            a.name,
            a.baseline,
            a.current,
            a.pct_change * 100.0,
            if a.regressed { "⛔" } else { "ok" }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(success: bool, usd: f64, wall: f64, quality: f64) -> EvalResult {
        EvalResult {
            goal: "g".into(),
            success,
            usd,
            wall_min: wall,
            quality,
            judged: false,
        }
    }

    #[test]
    fn summarize_averages() {
        let results = vec![result(true, 0.10, 5.0, 0.8), result(false, 0.30, 15.0, 0.6)];
        let s = summarize(&results);
        assert_eq!(s.n, 2);
        assert_eq!(s.success_rate, 0.5);
        assert!((s.avg_usd - 0.20).abs() < 1e-9);
        assert!((s.avg_wall - 10.0).abs() < 1e-9);
        assert!((s.avg_quality - 0.7).abs() < 1e-9);
    }

    #[test]
    fn summarize_empty() {
        let s = summarize(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.success_rate, 0.0);
    }

    #[test]
    fn no_change_is_not_a_regression() {
        let base = summarize(&[result(true, 0.1, 5.0, 0.9)]);
        let cur = base.clone();
        let r = compare(&cur, &base, 0.10);
        assert!(!r.regressed);
    }

    #[test]
    fn cost_increase_beyond_threshold_regresses() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_usd: 0.12,
            ..base.clone()
        }; // +20% > 10%
        let r = compare(&cur, &base, 0.10);
        assert!(r.regressed);
        assert!(
            r.axes
                .iter()
                .find(|a| a.name == "avg_usd")
                .unwrap()
                .regressed
        );
    }

    #[test]
    fn cost_increase_within_threshold_ok() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_usd: 0.105,
            ..base.clone()
        }; // +5%
        assert!(!compare(&cur, &base, 0.10).regressed);
    }

    #[test]
    fn success_rate_drop_regresses() {
        let base = EvalSummary {
            n: 10,
            success_rate: 0.90,
            avg_usd: 0.1,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            success_rate: 0.70,
            ..base.clone()
        }; // big drop
        assert!(compare(&cur, &base, 0.10).regressed);
    }

    #[test]
    fn improvements_never_regress() {
        let base = EvalSummary {
            n: 10,
            success_rate: 0.8,
            avg_usd: 0.2,
            avg_wall: 10.0,
            avg_quality: 0.7,
        };
        // Cheaper, faster, higher success + quality.
        let cur = EvalSummary {
            n: 10,
            success_rate: 0.95,
            avg_usd: 0.1,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let r = compare(&cur, &base, 0.10);
        assert!(!r.regressed);
    }

    #[test]
    fn render_report_marks_regression() {
        let base = EvalSummary {
            n: 1,
            success_rate: 1.0,
            avg_usd: 0.10,
            avg_wall: 5.0,
            avg_quality: 0.9,
        };
        let cur = EvalSummary {
            avg_wall: 20.0,
            ..base.clone()
        };
        let md = render_report(&compare(&cur, &base, 0.10));
        assert!(md.contains("REGRESSED"));
        assert!(md.contains("avg_wall"));
    }

    #[test]
    fn goals_file_mixes_plain_lines_and_golden_references() {
        let goals = parse_goals(
            "# suite\n\
             add a --version flag\n\
             \n\
             {\"goal\":\"fix the parser\",\"golden_commit\":\"abc123\"}\n\
             {\"goal\":\"rename it\",\"golden_diff\":\"rename.diff\",\"base\":\"v1\"}\n",
        )
        .unwrap();
        assert_eq!(goals.len(), 3);
        assert_eq!(goals[0].goal, "add a --version flag");
        assert_eq!(goals[0].base(), None);
        assert_eq!(goals[1].base().as_deref(), Some("abc123^"));
        assert_eq!(goals[2].base().as_deref(), Some("v1"));
        assert_eq!(goals[2].golden_diff, Some(PathBuf::from("rename.diff")));

        for bad in [
            "{\"goal\":\"x\",\"golden_commit\":\"a\",\"golden_diff\":\"b\"}",
            "{\"goal\":\"x\",\"golden_commit\":\"--output=/tmp/x\"}",
            "{\"goal\":\" \"}",
            "{\"goal\":\"x\",\"golden\":\"typo\"}",
            "{\"goal\":",
        ] {
            let err = parse_goals(&format!("ok\n{bad}")).unwrap_err();
            assert!(err.starts_with("line 2:"), "{bad}: {err}");
        }
    }

    /// The suite `.github/workflows/eval.yml` runs must parse, and pin every
    /// goal to a full golden commit: an abbreviated one can turn ambiguous
    /// as history grows.
    #[test]
    fn the_committed_goals_file_parses() {
        let goals = parse_goals(include_str!("../../../eval/goals.jsonl")).unwrap();
        assert!(!goals.is_empty());
        for g in &goals {
            let commit = g.golden_commit.as_deref().unwrap_or_default();
            assert!(
                commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
                "{:?} needs a full golden_commit",
                g.goal
            );
        }
    }

    /// `git diff <from> <to>` answers from a table; anything else fails.
    struct FakeGit(Vec<(&'static str, &'static str)>);

    impl CommandRunner for FakeGit {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            assert_eq!(program, "git");
            assert_eq!(args[..3], ["diff", "--no-color", "--no-ext-diff"]);
            assert_eq!(args[5], "--", "revisions end before the path separator");
            let hit = self.0.iter().find(|(from, _)| *from == args[3]);
            Ok(CommandOut {
                status: Some(if hit.is_some() { 0 } else { 128 }),
                stdout: hit.map(|(_, d)| d.to_string()).unwrap_or_default(),
                stderr: if hit.is_some() {
                    String::new()
                } else {
                    format!("fatal: bad revision '{}'", args[3])
                },
            })
        }
    }

    use crate::pr::CommandOut;

    struct Judge(Result<&'static str, ()>);

    #[async_trait::async_trait]
    impl PlannerLlm for Judge {
        async fn complete(
            &self,
            system: String,
            user: String,
        ) -> Result<String, wingman_core::WingmanError> {
            assert!(system.contains("reference change"));
            assert!(user.contains("Reference diff:\n+golden"), "{user}");
            assert!(user.contains("Candidate diff:\n+candidate"), "{user}");
            self.0
                .map(str::to_string)
                .map_err(|_| wingman_core::WingmanError::Other("offline".into()))
        }
    }

    fn golden_goal() -> EvalGoal {
        parse_goals("{\"goal\":\"g\",\"golden_commit\":\"gold\"}")
            .unwrap()
            .remove(0)
    }

    #[test]
    fn golden_diff_reads_a_commit_or_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let git = FakeGit(vec![("gold^", "+golden\n")]);
        let root = dir.path();
        assert_eq!(
            golden_diff(&git, root, root, &golden_goal()).unwrap(),
            Some("+golden\n".into())
        );

        std::fs::write(root.join("ref.diff"), "+from file\n").unwrap();
        let from_file = parse_goals("{\"goal\":\"g\",\"golden_diff\":\"ref.diff\"}")
            .unwrap()
            .remove(0);
        assert_eq!(
            golden_diff(&git, root, root, &from_file).unwrap(),
            Some("+from file\n".into())
        );

        let plain = parse_goals("g").unwrap().remove(0);
        assert_eq!(golden_diff(&git, root, root, &plain).unwrap(), None);

        let missing = parse_goals("{\"goal\":\"g\",\"golden_commit\":\"nope\"}")
            .unwrap()
            .remove(0);
        assert!(golden_diff(&git, root, root, &missing)
            .unwrap_err()
            .contains("bad revision"));
        let empty = FakeGit(vec![("gold^", "\n")]);
        assert!(golden_diff(&empty, root, root, &golden_goal()).is_err());
    }

    #[tokio::test]
    async fn judge_rejects_a_score_off_the_unit_scale() {
        let v = judge_diff(
            &Judge(Ok("Sure: {\"score\": 0.75, \"reason\": \"close\"}")),
            "g",
            "+golden",
            "+candidate",
        )
        .await
        .unwrap();
        assert_eq!(v.score, 0.75);
        assert_eq!(v.reason, "close");

        for reply in [Ok("{\"score\": 7}"), Ok("no json"), Err(())] {
            assert!(judge_diff(&Judge(reply), "g", "+golden", "+candidate")
                .await
                .is_err());
        }
        assert!(clip(&"é".repeat(10), 4).starts_with("éééé\n[... diff truncated]"));
    }

    async fn score(
        judge: Option<&dyn PlannerLlm>,
        git: &FakeGit,
        goal: EvalGoal,
        success: bool,
    ) -> QualityScore {
        let root = Path::new(".");
        let range = Some(("base", "wingman/auto/r1"));
        score_quality(judge, git, root, root, &goal, success, range).await
    }

    #[tokio::test]
    async fn quality_is_judged_against_a_golden_and_proxied_without_one() {
        let git = FakeGit(vec![("gold^", "+golden\n"), ("base", "+candidate\n")]);
        let judge = Judge(Ok("{\"score\": 0.6, \"reason\": \"half the tests\"}"));
        let offline = Judge(Err(()));

        assert_eq!(
            score(Some(&judge), &git, golden_goal(), true).await,
            QualityScore {
                quality: 0.6,
                judged: true,
                note: Some("half the tests".into())
            }
        );

        // No golden reference: the success proxy, without asking the judge.
        let plain = parse_goals("g").unwrap().remove(0);
        let proxied = score(Some(&offline), &git, plain, true).await;
        assert_eq!((proxied.quality, proxied.judged), (1.0, false));

        // A failed run scores 0 whatever the reference.
        let failed = score(Some(&judge), &git, golden_goal(), false).await;
        assert_eq!((failed.quality, failed.judged), (0.0, false));

        // A judge that cannot answer, or none at all, falls back to the proxy
        // and says why.
        let fell_back = score(Some(&offline), &git, golden_goal(), true).await;
        assert_eq!((fell_back.quality, fell_back.judged), (1.0, false));
        assert!(fell_back.note.unwrap().contains("judge call failed"));
        let no_judge = score(None, &git, golden_goal(), true).await;
        assert_eq!((no_judge.quality, no_judge.judged), (1.0, false));

        // A run that changed nothing is judged 0 without a call.
        let empty = FakeGit(vec![("gold^", "+golden\n"), ("base", "")]);
        let nothing = score(Some(&offline), &empty, golden_goal(), true).await;
        assert_eq!((nothing.quality, nothing.judged), (0.0, true));
    }
}
