//! `wingman review <pr#> --comment` — post the review's findings back to the
//! PR as one GitHub review, with each finding anchored inline to its diff line.
//!
//! Findings come from the reviewer in the `severity|file:line|message` line
//! format `review-multi` already parses. A finding whose `file:line` is a
//! commentable line on the new side of the diff (an added or context line)
//! becomes an inline comment; anything else is listed in the review body so
//! it isn't lost. Everything goes through `gh api` in a single
//! `POST .../pulls/<n>/reviews`, so the PR gets one notification, not one per
//! finding.
//!
//! Every body wingman posts carries [`MARKER`] and renders each finding as
//! ``**severity** `file:line`: message``. On the next run the reviews the `gh`
//! user posted on the PR (their bodies and inline comments) are read back, and
//! a finding whose `(file, message)` was already posted is dropped. Only the
//! `gh` user's reviews count: anyone can paste the marker, and the PR author
//! must not be able to pre-empt a finding that way. The line is left out of
//! the key so a push that shifts the code doesn't re-post the same finding.
//! `--dry-run` does all of that and prints the payload instead of posting.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use wingman_autonomous::pr::CommandRunner;

use super::review_multi::{normalize_message, parse_finding};

/// Tags every body wingman posts, so dedup only ever matches its own output.
const MARKER: &str = "<!-- wingman-review -->";

// ponytail: first 100 inline comments per review; a wingman review past that
// re-posts the overflow. Page `comments` when one shows up.
const REVIEWS_QUERY: &str = "query($url: URI!, $endCursor: String) { resource(url: $url) { \
    ... on PullRequest { reviews(first: 100, after: $endCursor) { \
    pageInfo { hasNextPage endCursor } \
    nodes { viewerDidAuthor body comments(first: 100) { nodes { body } } } } } } }";

/// One review per line, so the pages `--paginate` prints read as one list.
const REVIEWS_JQ: &str = ".data.resource.reviews.nodes[] | @json";

/// The PR as `gh api` needs to address it, resolved from whatever the user
/// passed (`42`, a branch, or a PR URL — anything `gh pr view` accepts).
#[derive(Debug, PartialEq)]
pub struct PrTarget {
    host: String,
    url: String,
    /// `repos/<owner>/<repo>/pulls/<number>`
    api_path: String,
    /// The head commit the review is pinned to.
    head_sha: String,
}

/// Resolve `pr` to its repo, number and head commit. Called before the diff
/// is fetched, so the review is pinned to a commit no newer than the diff.
pub fn resolve_pr(runner: &dyn CommandRunner, cwd: &Path, pr: &str) -> Result<PrTarget> {
    let out = runner
        .run(
            "gh",
            &["pr", "view", pr, "--json", "number,headRefOid,url"],
            cwd,
        )
        .context("running `gh pr view` — is the GitHub CLI installed?")?;
    if !out.success() {
        anyhow::bail!("`gh pr view {pr}` failed: {}", out.stderr.trim());
    }
    let v: Value = serde_json::from_str(&out.stdout).context("parsing `gh pr view` output")?;
    let (Some(number), Some(head), Some(url)) = (
        v["number"].as_u64(),
        v["headRefOid"].as_str(),
        v["url"].as_str(),
    ) else {
        anyhow::bail!("`gh pr view {pr}` returned no number/headRefOid/url");
    };
    // https://<host>/<owner>/<repo>/pull/<n>
    let parts: Vec<&str> = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .collect();
    let [host, owner, repo, ..] = parts[..] else {
        anyhow::bail!("unrecognised PR url '{url}'");
    };
    Ok(PrTarget {
        host: host.to_string(),
        url: url.to_string(),
        api_path: format!("repos/{owner}/{repo}/pulls/{number}"),
        head_sha: head.to_string(),
    })
}

/// Read the reviewer output, drop what's already on the PR, and post the rest
/// as one review (or print the payload on `dry_run`). Returns the line to show
/// the user.
pub fn post(
    runner: &dyn CommandRunner,
    cwd: &Path,
    target: &PrTarget,
    diff: &str,
    reviewer_text: &str,
    dry_run: bool,
) -> Result<String> {
    let bodies = own_review_bodies(runner, cwd, target)?;
    let Some(payload) = build_payload(reviewer_text, diff, &posted_keys(&bodies), target) else {
        return Ok("wingman: no new findings to post".into());
    };
    let pretty = serde_json::to_string_pretty(&payload)?;
    if dry_run {
        println!("POST {}/reviews\n{pretty}", target.api_path);
        return Ok("wingman: dry run — nothing posted".into());
    }

    // `gh api` takes a nested JSON body only via `--input`; a private temp
    // file keeps the payload off the command line.
    let mut file = tempfile::NamedTempFile::new().context("creating review payload file")?;
    file.write_all(pretty.as_bytes())?;
    file.flush()?;
    let input = file.path().to_string_lossy().into_owned();
    let endpoint = format!("{}/reviews", target.api_path);
    let out = runner.run(
        "gh",
        &[
            "api",
            "--hostname",
            &target.host,
            "--method",
            "POST",
            &endpoint,
            "--input",
            &input,
            "--jq",
            ".html_url",
        ],
        cwd,
    )?;
    if !out.success() {
        anyhow::bail!("posting the review failed: {}", out.stderr.trim());
    }
    Ok(format!("wingman: posted review {}", out.stdout.trim()))
}

/// The bodies of every review the `gh` user posted on the PR, and of those
/// reviews' inline comments.
fn own_review_bodies(
    runner: &dyn CommandRunner,
    cwd: &Path,
    target: &PrTarget,
) -> Result<Vec<String>> {
    let query = format!("query={REVIEWS_QUERY}");
    let url = format!("url={}", target.url);
    let out = runner.run(
        "gh",
        &[
            "api",
            "graphql",
            "--hostname",
            &target.host,
            "--paginate",
            "-f",
            &query,
            "-f",
            &url,
            "--jq",
            REVIEWS_JQ,
        ],
        cwd,
    )?;
    // Fail closed: without the existing reviews we can't dedup, and posting
    // duplicates on every run is the thing this exists to avoid.
    if !out.success() {
        anyhow::bail!("listing the PR's reviews failed: {}", out.stderr.trim());
    }
    let mut bodies = Vec::new();
    for line in out.stdout.lines() {
        let Ok(review) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if review["viewerDidAuthor"] != true {
            continue;
        }
        let comments = review["comments"]["nodes"].as_array().into_iter().flatten();
        for body in std::iter::once(&review["body"]).chain(comments.map(|c| &c["body"])) {
            bodies.extend(body.as_str().map(str::to_string));
        }
    }
    Ok(bodies)
}

/// `(file, normalized message)` for every finding in a body wingman posted.
fn posted_keys(bodies: &[String]) -> HashSet<(String, String)> {
    let mut keys = HashSet::new();
    for body in bodies.iter().filter(|b| b.contains(MARKER)) {
        for line in body.lines() {
            let line = line.trim().trim_start_matches("- ");
            let Some(rest) = line.strip_prefix("**") else {
                continue;
            };
            let Some((_, rest)) = rest.split_once("** `") else {
                continue;
            };
            let Some((loc, message)) = rest.split_once("`: ") else {
                continue;
            };
            let file = loc.rsplit_once(':').map_or(loc, |(f, _)| f);
            keys.insert((file.to_string(), normalize_message(message)));
        }
    }
    keys
}

/// `(path, line)` for every line GitHub accepts a `side: RIGHT` comment on.
fn commentable_lines(diff: &str) -> HashSet<(String, usize)> {
    let mut lines = HashSet::new();
    for file in super::diff::parse_unified_diff(diff) {
        for hunk in &file.hunks {
            for offset in 0..hunk.new_block.len() {
                lines.insert((file.new_path.clone(), hunk.new_start + offset));
            }
        }
    }
    lines
}

/// The `POST .../reviews` body, or `None` when nothing new is left to post.
fn build_payload(
    reviewer_text: &str,
    diff: &str,
    posted: &HashSet<(String, String)>,
    target: &PrTarget,
) -> Option<Value> {
    let anchors = commentable_lines(diff);
    let mut seen = posted.clone();
    let mut comments = Vec::new();
    let mut unanchored = Vec::new();
    for f in reviewer_text.lines().filter_map(parse_finding) {
        if f.severity == "ok" || !seen.insert((f.file.clone(), normalize_message(&f.message))) {
            continue;
        }
        let rendered = format!("**{}** `{}:{}`: {}", f.severity, f.file, f.line, f.message);
        match f.line.parse::<usize>() {
            Ok(line) if anchors.contains(&(f.file.clone(), line)) => comments.push(json!({
                "path": f.file,
                "line": line,
                "side": "RIGHT",
                "body": format!("{rendered}\n\n{MARKER}"),
            })),
            _ => unanchored.push(format!("- {rendered}")),
        }
    }
    let total = comments.len() + unanchored.len();
    if total == 0 {
        return None;
    }
    let mut body = format!("wingman review: {total} new finding(s).\n");
    if !unanchored.is_empty() {
        body.push_str("\nNot on a diff line:\n\n");
        body.push_str(&unanchored.join("\n"));
        body.push('\n');
    }
    body.push_str(&format!("\n{MARKER}"));
    Some(json!({
        "commit_id": target.head_sha,
        "event": "COMMENT",
        "body": body,
        "comments": comments,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wingman_autonomous::pr::CommandOut;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,3 +10,4 @@ fn main() {
 let a = 1;
+let b = a.unwrap();
 let c = 3;
 let d = 4;
";

    /// Records every call and answers from canned stdout, like the mock gh
    /// runners in `wingman-autonomous`. The POST's `--input` file is read at
    /// call time, since it's deleted once `post` returns.
    struct MockGh {
        view: &'static str,
        /// What the reviews query prints after `--jq`: one review per line.
        reviews: String,
        calls: Mutex<Vec<Vec<String>>>,
        posted: Mutex<Option<Value>>,
    }

    impl MockGh {
        fn new(reviews: &[Value]) -> Self {
            Self {
                view: r#"{"number":42,"headRefOid":"abc123","url":"https://github.com/o/r/pull/42"}"#,
                reviews: reviews.iter().map(|r| format!("{r}\n")).collect(),
                calls: Mutex::new(Vec::new()),
                posted: Mutex::new(None),
            }
        }
        fn posts(&self) -> usize {
            let calls = self.calls.lock().unwrap();
            calls.iter().filter(|a| a.contains(&"POST".into())).count()
        }
    }

    impl CommandRunner for MockGh {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            assert_eq!(program, "gh");
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            let stdout = if args[..2] == ["pr", "view"] {
                self.view.to_string()
            } else if args.contains(&"POST") {
                let i = args.iter().position(|a| *a == "--input").unwrap();
                let text = std::fs::read_to_string(args[i + 1]).unwrap();
                *self.posted.lock().unwrap() = Some(serde_json::from_str(&text).unwrap());
                "https://github.com/o/r/pull/42#pullrequestreview-1\n".to_string()
            } else if args[..2] == ["api", "graphql"] {
                assert!(args.contains(&"url=https://github.com/o/r/pull/42"));
                assert!(args.contains(&"github.com"));
                self.reviews.clone()
            } else {
                panic!("unexpected gh call {args:?}");
            };
            Ok(CommandOut {
                status: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }

    fn target(gh: &MockGh) -> PrTarget {
        resolve_pr(gh, Path::new("."), "42").unwrap()
    }

    #[test]
    fn resolve_pr_reads_repo_number_and_head_from_the_url() {
        let gh = MockGh::new(&[]);
        assert_eq!(
            target(&gh),
            PrTarget {
                host: "github.com".into(),
                url: "https://github.com/o/r/pull/42".into(),
                api_path: "repos/o/r/pulls/42".into(),
                head_sha: "abc123".into(),
            }
        );
    }

    #[test]
    fn posts_one_review_with_inline_and_unanchored_findings() {
        let gh = MockGh::new(&[]);
        let text = "Some preamble.\n\
                    major|src/lib.rs:11|unwrap on a value that can be None\n\
                    minor|src/other.rs:5|name shadows the import\n\
                    ok|-:-|no findings\n";
        let msg = post(&gh, Path::new("."), &target(&gh), DIFF, text, false).unwrap();
        assert!(msg.contains("pullrequestreview-1"), "{msg}");

        let payload = gh.posted.lock().unwrap().clone().unwrap();
        assert_eq!(payload["commit_id"], "abc123");
        assert_eq!(payload["event"], "COMMENT");
        let comments = payload["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["path"], "src/lib.rs");
        assert_eq!(comments[0]["line"], 11);
        assert_eq!(comments[0]["side"], "RIGHT");
        let body = payload["body"].as_str().unwrap();
        assert!(body.contains("2 new finding(s)"), "{body}");
        assert!(body.contains("- **minor** `src/other.rs:5`: name shadows"));
        assert!(body.contains(MARKER));
        // The endpoint and host came from the resolved PR.
        let calls = gh.calls.lock().unwrap();
        let post_call = calls.iter().find(|a| a.contains(&"POST".into())).unwrap();
        assert!(post_call.contains(&"repos/o/r/pulls/42/reviews".into()));
        assert!(post_call.contains(&"github.com".into()));
    }

    #[test]
    fn a_line_outside_the_diff_goes_to_the_body() {
        let gh = MockGh::new(&[]);
        // Line 20 is past the hunk's new-side range (10..=13).
        let text = "major|src/lib.rs:20|off the diff\n";
        post(&gh, Path::new("."), &target(&gh), DIFF, text, false).unwrap();
        let payload = gh.posted.lock().unwrap().clone().unwrap();
        assert!(payload["comments"].as_array().unwrap().is_empty());
        assert!(payload["body"]
            .as_str()
            .unwrap()
            .contains("`src/lib.rs:20`: off the diff"));
    }

    #[test]
    fn findings_posted_by_a_previous_run_are_not_reposted() {
        // An earlier wingman review: one body entry and one inline comment
        // whose line has since moved. Neither an unmarked comment in it nor
        // someone else's review pasting the marker may suppress anything.
        let gh = MockGh::new(&[
            json!({
                "viewerDidAuthor": true,
                "body": "wingman review: 2 new finding(s).\n\nNot on a diff line:\n\n- **minor** `src/other.rs:5`: name shadows the import\n\n<!-- wingman-review -->",
                "comments": {"nodes": [
                    {"body": "**major** `src/lib.rs:12`: Unwrap on a value that can be None\n\n<!-- wingman-review -->"},
                    {"body": "**minor** `src/lib.rs:13`: d is unused"},
                ]},
            }),
            json!({
                "viewerDidAuthor": false,
                "body": "**minor** `src/lib.rs:13`: d is unused\n\n<!-- wingman-review -->",
                "comments": {"nodes": []},
            }),
        ]);
        let text = "major|src/lib.rs:11|unwrap on a value that can be None\n\
                    minor|src/other.rs:5|name shadows the import\n\
                    minor|src/lib.rs:13|d is unused\n\
                    minor|src/lib.rs:13|d is unused\n";
        post(&gh, Path::new("."), &target(&gh), DIFF, text, false).unwrap();
        let payload = gh.posted.lock().unwrap().clone().unwrap();
        let comments = payload["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "{payload}");
        assert_eq!(comments[0]["line"], 13);
        assert!(payload["body"]
            .as_str()
            .unwrap()
            .contains("1 new finding(s)"));
    }

    #[test]
    fn nothing_new_posts_nothing() {
        let gh = MockGh::new(&[json!({
            "viewerDidAuthor": true,
            "body": "",
            "comments": {"nodes": [
                {"body": "**major** `src/lib.rs:11`: unwrap on a value that can be None\n\n<!-- wingman-review -->"},
            ]},
        })]);
        let text = "major|src/lib.rs:11|unwrap on a value that can be None\nok|-:-|no findings\n";
        let msg = post(&gh, Path::new("."), &target(&gh), DIFF, text, false).unwrap();
        assert_eq!(msg, "wingman: no new findings to post");
        assert_eq!(gh.posts(), 0);
    }

    #[test]
    fn dry_run_reads_existing_comments_but_never_posts() {
        let gh = MockGh::new(&[]);
        let text = "major|src/lib.rs:11|unwrap on a value that can be None\n";
        let msg = post(&gh, Path::new("."), &target(&gh), DIFF, text, true).unwrap();
        assert!(msg.contains("dry run"));
        assert_eq!(gh.posts(), 0);
        assert_eq!(gh.calls.lock().unwrap().len(), 2); // view + reviews
    }

    #[test]
    fn a_failed_comment_listing_fails_closed() {
        struct Failing;
        impl CommandRunner for Failing {
            fn run(&self, _: &str, _: &[&str], _: &Path) -> std::io::Result<CommandOut> {
                Ok(CommandOut {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "HTTP 403".into(),
                })
            }
        }
        let gh = MockGh::new(&[]);
        let err = post(
            &Failing,
            Path::new("."),
            &target(&gh),
            DIFF,
            "major|src/lib.rs:11|x\n",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 403"), "{err}");
    }
}
