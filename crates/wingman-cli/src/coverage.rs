//! Changed-line coverage gate stage (`[verify].coverage`): run the project's
//! coverage tool, then report how many of the lines this turn changed the
//! tests actually executed. Proves the tests exercise the change, which a
//! green `cargo test` alone does not.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use wingman_core::{GateReport, TurnGate};

use crate::runtime::{changed_lines_by_file, changed_paths, changed_rust_crates, run_check_cmd};

/// Report file (as the report spells it, `/`-separated) -> line -> hit count.
type Coverage = HashMap<String, BTreeMap<u32, u64>>;

pub struct CoverageGate {
    pub root: std::path::PathBuf,
    /// "auto" or a command containing `{out}`.
    pub cmd: String,
    /// `[verify].min_changed_line_coverage`; `None` reports without failing.
    pub min: Option<f64>,
}

#[async_trait::async_trait]
impl TurnGate for CoverageGate {
    fn label(&self) -> String {
        "changed-line coverage".into()
    }

    async fn check(&self) -> GateReport {
        let skip = |why: String| GateReport {
            passed: true,
            summary: format!("coverage: ⚠ skipped ({why})"),
        };
        let changed = turn_changed_lines(&self.root);
        if changed.is_empty() {
            return GateReport {
                passed: true,
                summary: "coverage: none (no changed lines)".into(),
            };
        }
        let (program, cmd) = if self.cmd == "auto" {
            match detect_coverage_cmd(&self.root) {
                Some(found) => found,
                None => return skip("no coverage tool known for this project".into()),
            }
        } else {
            let program = self.cmd.split_whitespace().next().unwrap_or("").to_string();
            (program, self.cmd.clone())
        };
        // A missing tool is an environment gap, not something the agent can
        // fix by editing — never fail the gate for it.
        if wingman_lsp::server::which_on_path(&program).is_none() {
            return skip(format!("`{program}` not on PATH"));
        }

        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => return skip(format!("no temp dir: {e}")),
        };
        let out = dir.path().join("lcov.info");
        let cmd = cmd
            .replace("{out}", &format!("\"{}\"", out.display()))
            .replace("{dir}", &format!("\"{}\"", dir.path().display()));
        let run = run_check_cmd(&cmd, &self.root).await;
        let Ok(text) = std::fs::read_to_string(&out) else {
            return skip(format!("the coverage run wrote no report\n{}", run.summary));
        };

        let result = intersect(&parse_coverage(&text), &changed);
        let passed = meets_threshold(result.covered, result.total, self.min);
        GateReport {
            passed,
            summary: format!("coverage: {}", render(&result, self.min, passed)),
        }
    }
}

/// This turn's changed lines: tracked edits from the diff, plus every line of
/// an untracked file (a new file is all new). ponytail: an untracked file
/// inside an untracked directory is missed (`git status` names the directory
/// only); `-uall` if that gap matters.
pub(crate) fn turn_changed_lines(root: &Path) -> HashMap<String, BTreeSet<u32>> {
    let mut map = changed_lines_by_file(root);
    for path in changed_paths(root) {
        if map.contains_key(&path) {
            continue;
        }
        // Fails for directories and deleted files, which is the point.
        if let Ok(text) = std::fs::read_to_string(root.join(&path)) {
            map.insert(path, (1..=text.lines().count() as u32).collect());
        }
    }
    map
}

/// The coverage command for this project and the program it needs on PATH,
/// with `{out}` for the report file (`{dir}` for its directory). Same marker
/// files, same order, as [`crate::runtime::detect_turn_gate_cmd`].
fn detect_coverage_cmd(root: &Path) -> Option<(String, String)> {
    let (program, cmd) = if root.join("Cargo.toml").exists() {
        // Only the changed crates: their files are the ones being measured.
        let pkgs: String = changed_rust_crates(root)
            .iter()
            .map(|c| format!(" -p {c}"))
            .collect();
        (
            "cargo-llvm-cov",
            format!("cargo llvm-cov{pkgs} --lcov --output-path {{out}}"),
        )
    } else if root.join("tsconfig.json").exists() || root.join("package.json").exists() {
        // ponytail: PATH only — a c8/nyc installed just in node_modules/.bin
        // isn't found, and the stage skips.
        if wingman_lsp::server::which_on_path("c8").is_some() {
            (
                "c8",
                "c8 --reporter=lcovonly --reports-dir {dir} npm test".into(),
            )
        } else {
            (
                "nyc",
                "nyc --reporter=lcovonly --report-dir {dir} npm test".into(),
            )
        }
    } else if root.join("go.mod").exists() {
        ("go", "go test -coverprofile={out} ./...".into())
    } else if root.join("pyproject.toml").exists() || root.join("setup.py").exists() {
        ("pytest", "pytest -q --cov --cov-report=lcov:{out}".into())
    } else {
        return None;
    };
    Some((program.to_string(), cmd))
}

/// Parse a Go coverprofile (starts `mode:`) or an lcov tracefile.
fn parse_coverage(text: &str) -> Coverage {
    if text.trim_start().starts_with("mode:") {
        parse_go_profile(text)
    } else {
        parse_lcov(text)
    }
}

/// lcov: `SF:<file>` opens a record, `DA:<line>,<hits>[,<checksum>]` per line.
fn parse_lcov(text: &str) -> Coverage {
    let mut cov = Coverage::new();
    let mut file: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if let Some(path) = line.strip_prefix("SF:") {
            file = Some(path.replace('\\', "/"));
        } else if line == "end_of_record" {
            file = None;
        } else if let (Some(da), Some(f)) = (line.strip_prefix("DA:"), &file) {
            let mut it = da.split(',');
            let line_no = it.next().and_then(|s| s.parse::<u32>().ok());
            let hits = it.next().and_then(|s| s.parse::<u64>().ok());
            if let (Some(l), Some(h)) = (line_no, hits) {
                let slot = cov.entry(f.clone()).or_default().entry(l).or_insert(0);
                *slot = slot.saturating_add(h);
            }
        }
    }
    cov
}

/// Go: `<import/path/file.go>:<l0>.<c0>,<l1>.<c1> <stmts> <count>` per block.
/// A line counts as run when any block spanning it ran.
fn parse_go_profile(text: &str) -> Coverage {
    let mut cov = Coverage::new();
    for line in text.lines().skip_while(|l| !l.starts_with("mode:")).skip(1) {
        let Some((file, rest)) = line.rsplit_once(':') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let (Some(span), Some(_stmts), Some(count)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let line_of = |pos: &str| pos.split('.').next().and_then(|l| l.parse::<u32>().ok());
        let Some((a, b)) = span.split_once(',') else {
            continue;
        };
        let (Some(start), Some(end), Ok(count)) = (line_of(a), line_of(b), count.parse::<u64>())
        else {
            continue;
        };
        let lines = cov.entry(file.to_string()).or_default();
        for l in start..=end {
            let slot = lines.entry(l).or_insert(0);
            *slot = (*slot).max(count);
        }
    }
    cov
}

#[derive(Debug, PartialEq)]
struct ChangedCoverage {
    covered: usize,
    /// Changed lines the report instruments (blank lines and comments aren't).
    total: usize,
    uncovered: Vec<(String, u32)>,
}

/// Changed lines against the report. A report path matches a repo-relative
/// file when equal or ending in `/<file>` — lcov paths are usually absolute
/// and Go's are import paths. ponytail: suffix match, so two files sharing a
/// long relative suffix under different roots could cross; fine per repo.
fn intersect(cov: &Coverage, changed: &HashMap<String, BTreeSet<u32>>) -> ChangedCoverage {
    let mut result = ChangedCoverage {
        covered: 0,
        total: 0,
        uncovered: Vec::new(),
    };
    let mut files: Vec<_> = changed.iter().collect();
    files.sort();
    for (rel, lines) in files {
        let suffix = format!("/{rel}");
        let Some(hits) = cov
            .iter()
            .find(|(k, _)| *k == rel || k.ends_with(&suffix))
            .map(|(_, v)| v)
        else {
            continue;
        };
        for l in lines {
            match hits.get(l) {
                Some(0) => {
                    result.total += 1;
                    result.uncovered.push((rel.clone(), *l));
                }
                Some(_) => {
                    result.total += 1;
                    result.covered += 1;
                }
                None => {}
            }
        }
    }
    result
}

/// Unset threshold never fails; nothing instrumentable changed never fails.
fn meets_threshold(covered: usize, total: usize, min: Option<f64>) -> bool {
    match min {
        None => true,
        Some(_) if total == 0 => true,
        Some(m) => covered as f64 / total as f64 >= m,
    }
}

fn render(r: &ChangedCoverage, min: Option<f64>, passed: bool) -> String {
    if r.total == 0 {
        return "✓ no changed line is instrumented in the report".into();
    }
    let mark = if passed { "✓" } else { "✗" };
    let mut s = format!("{mark} {}/{} changed lines covered", r.covered, r.total);
    if let Some(m) = min {
        let pct = r.covered as f64 * 100.0 / r.total as f64;
        let cmp = if passed { "≥" } else { "<" };
        s.push_str(&format!(" ({pct:.1}% {cmp} {:.0}% minimum)", m * 100.0));
    }
    let ranges = line_ranges(&r.uncovered);
    if !ranges.is_empty() {
        let more = ranges.len().saturating_sub(10);
        s.push_str("\nuncovered: ");
        s.push_str(&ranges[..ranges.len().min(10)].join(", "));
        if more > 0 {
            s.push_str(&format!(" (+{more} more)"));
        }
    }
    s
}

/// `a.rs:3-5` for consecutive lines of one file; input sorted by file, line.
fn line_ranges(lines: &[(String, u32)]) -> Vec<String> {
    let mut out: Vec<(String, u32, u32)> = Vec::new();
    for (file, l) in lines {
        match out.last_mut() {
            Some((f, _, end)) if f == file && *end + 1 == *l => *end = *l,
            _ => out.push((file.clone(), *l, *l)),
        }
    }
    out.into_iter()
        .map(|(f, a, b)| {
            if a == b {
                format!("{f}:{a}")
            } else {
                format!("{f}:{a}-{b}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(entries: &[(&str, &[u32])]) -> HashMap<String, BTreeSet<u32>> {
        entries
            .iter()
            .map(|(f, ls)| (f.to_string(), ls.iter().copied().collect()))
            .collect()
    }

    #[test]
    fn parses_lcov_records() {
        let text = "TN:\nSF:C:\\repo\\src\\lib.rs\nFN:1,f\nDA:1,3\nDA:2,0\nDA:4,1,abc\nend_of_record\nSF:/repo/src/b.rs\nDA:9,0\nend_of_record\n";
        let cov = parse_coverage(text);
        assert_eq!(
            cov["C:/repo/src/lib.rs"],
            BTreeMap::from([(1, 3), (2, 0), (4, 1)])
        );
        assert_eq!(cov["/repo/src/b.rs"], BTreeMap::from([(9, 0)]));
    }

    #[test]
    fn parses_go_profile_blocks_onto_lines() {
        let text = "mode: set\nexample.com/app/pkg/x.go:3.14,5.2 2 1\nexample.com/app/pkg/x.go:5.2,7.3 1 0\n";
        let cov = parse_coverage(text);
        assert_eq!(
            cov["example.com/app/pkg/x.go"],
            BTreeMap::from([(3, 1), (4, 1), (5, 1), (6, 0), (7, 0)])
        );
    }

    #[test]
    fn intersects_changed_lines_by_path_suffix() {
        let cov = parse_coverage(
            "SF:/abs/repo/src/lib.rs\nDA:1,1\nDA:2,0\nDA:3,0\nDA:5,2\nend_of_record\n",
        );
        // Line 4 isn't instrumented (a comment, say): not counted either way.
        // other.rs has no record at all.
        let r = intersect(
            &cov,
            &changed(&[("src/lib.rs", &[1, 2, 3, 4, 5]), ("other.rs", &[1])]),
        );
        assert_eq!(r.covered, 2);
        assert_eq!(r.total, 4);
        assert_eq!(line_ranges(&r.uncovered), ["src/lib.rs:2-3".to_string()]);
        // A suffix match is on whole path segments, not any trailing text.
        assert_eq!(intersect(&cov, &changed(&[("ib.rs", &[1])])).total, 0);
    }

    #[test]
    fn threshold_is_report_only_when_unset() {
        assert!(meets_threshold(0, 10, None));
        assert!(meets_threshold(7, 10, Some(0.7)));
        assert!(!meets_threshold(6, 10, Some(0.7)));
        assert!(meets_threshold(0, 0, Some(1.0)));
    }

    #[test]
    fn receipt_names_uncovered_ranges() {
        let r = ChangedCoverage {
            covered: 12,
            total: 14,
            uncovered: vec![("a.rs".into(), 3), ("a.rs".into(), 4)],
        };
        assert_eq!(
            render(&r, None, true),
            "✓ 12/14 changed lines covered\nuncovered: a.rs:3-4"
        );
        assert!(render(&r, Some(0.9), false)
            .starts_with("✗ 12/14 changed lines covered (85.7% < 90% minimum)"));
    }
}
