//! `trace_ref`: Chain of Recall over pilot runs.
//!
//! Every pilot run already logs the IDs of its neighbours: the goal, task
//! ids, worker session ids, commit shas, the PR url, and (for daemon-started
//! runs) the issue or check that caused it. This tool finds the runs that
//! mention an id and prints those neighbours, so the agent can rebuild an old
//! task's context one hop at a time ("that auth fix from Tuesday" → run →
//! PR → worker session) instead of searching blind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::filesystem::FileSystem;
use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

/// Newest runs first; older matches are named but not expanded.
const MAX_RUNS: usize = 5;

pub struct TraceRef {
    project_root: PathBuf,
}

impl TraceRef {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Debug, Deserialize)]
struct Args {
    id: String,
}

#[async_trait]
impl Tool for TraceRef {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "trace_ref".into(),
            description: "Find the pilot runs that mention an id (run id, task id, worker \
                          session id, commit sha, PR url or number, or an issue source like \
                          `github_issue#12`) and list each run's linked ids: goal, status, PR, \
                          tasks with their sessions and commits, and what started it. Call it \
                          again on any of those ids to hop further back; use read_session on a \
                          session id for the transcript."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Any id to trace, at least 3 characters." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let id = args.id.trim().to_string();
        if id.chars().count() < 3 {
            return ToolOutcome::err("id must be at least 3 characters".to_string());
        }
        ToolOutcome::ok(trace(ctx.fs.as_ref(), &self.project_root, &id).await)
    }
}

async fn trace(fs: &dyn FileSystem, root: &Path, id: &str) -> String {
    let wm = root.join(".wingman");
    let mut out = String::new();

    // What the daemon started, and from where: `{source, title, run_id}`.
    let mut started_by: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(queue) = fs.read_to_string(&wm.join("daemon-queue.jsonl")).await {
        for v in queue
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            if let (Some(run), Some(src)) = (v["run_id"].as_str(), v["source"].as_str()) {
                started_by.insert(run.to_string(), src.to_string());
            }
        }
    }

    let mut runs: Vec<(String, String)> = Vec::new();
    for e in fs
        .read_dir(&wm.join("autonomous"))
        .await
        .unwrap_or_default()
    {
        let Some(name) = e.path.file_name() else {
            continue;
        };
        let run_id = name.to_string_lossy().into_owned();
        let Ok(log) = fs.read_to_string(&e.path.join("tasks.jsonl")).await else {
            continue;
        };
        if run_id.contains(id)
            || log.contains(id)
            || started_by.get(&run_id).is_some_and(|s| s.contains(id))
        {
            runs.push((run_id, log));
        }
    }
    // Run ids start with a timestamp, so name order is age order.
    runs.sort_by(|a, b| b.0.cmp(&a.0));

    if runs.is_empty() {
        return format!(
            "No pilot run mentions `{id}`. For chat sessions, try recall_session instead."
        );
    }
    for (run_id, log) in runs.iter().take(MAX_RUNS) {
        out.push_str(&summarize(run_id, log, started_by.get(run_id)));
        out.push('\n');
    }
    if runs.len() > MAX_RUNS {
        let rest: Vec<&str> = runs[MAX_RUNS..].iter().map(|(r, _)| r.as_str()).collect();
        out.push_str(&format!(
            "Older runs also mention it: {}\n",
            rest.join(", ")
        ));
    }
    out
}

#[derive(Default)]
struct TaskLine {
    title: String,
    status: String,
    agent: String,
    commits: Vec<String>,
}

fn summarize(run_id: &str, log: &str, source: Option<&String>) -> String {
    let mut goal = String::new();
    let mut status = String::from("unknown");
    let mut prs: Vec<String> = Vec::new();
    let mut tasks: BTreeMap<String, TaskLine> = BTreeMap::new();
    let mut sessions: BTreeMap<String, String> = BTreeMap::new();
    for v in log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        match v["ev"].as_str().unwrap_or_default() {
            "run.start" => goal = s("goal"),
            "run.status" => status = s("status"),
            "run.pr" => prs.push(s("url")),
            "task.create" => tasks.entry(s("id")).or_default().title = s("title"),
            "task.status" => tasks.entry(s("id")).or_default().status = s("status"),
            "task.assign" => tasks.entry(s("id")).or_default().agent = s("agent"),
            "task.commit" => {
                let sha: String = s("sha").chars().take(10).collect();
                tasks.entry(s("id")).or_default().commits.push(sha);
            }
            "agent.spawn" => {
                if let Some(sid) = v["session_id"].as_str() {
                    sessions.insert(s("agent"), sid.to_string());
                }
            }
            _ => {}
        }
    }
    let goal: String = goal.chars().take(300).collect();
    let mut out = format!("run {run_id} [{status}]\n  goal: {goal}\n");
    if let Some(src) = source {
        out.push_str(&format!("  started by: {src}\n"));
    }
    for pr in &prs {
        out.push_str(&format!("  pr: {pr}\n"));
    }
    for (id, t) in &tasks {
        out.push_str(&format!("  task {id} [{}] {}", t.status, t.title));
        if let Some(sid) = sessions.get(&t.agent) {
            out.push_str(&format!(" session={sid}"));
        }
        if !t.commits.is_empty() {
            out.push_str(&format!(" commits={}", t.commits.join(",")));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hops_from_an_issue_to_the_run_its_pr_and_the_worker_session() {
        let dir = tempfile::tempdir().unwrap();
        let wm = dir.path().join(".wingman");
        let run = wm.join("autonomous").join("20261101-aaaa");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("tasks.jsonl"),
            [
                r#"{"ev":"run.start","t":"","run_id":"20261101-aaaa","goal":"fix login","base_commit":"b","integration_branch":"i"}"#,
                r#"{"ev":"task.create","t":"","id":"t1","role":"developer","title":"patch auth"}"#,
                r#"{"ev":"agent.spawn","t":"","agent":"a1","role":"developer","session_id":"sess-42"}"#,
                r#"{"ev":"task.assign","t":"","id":"t1","agent":"a1","worktree":"w"}"#,
                r#"{"ev":"task.commit","t":"","id":"t1","sha":"abcdef1234567"}"#,
                r#"{"ev":"task.status","t":"","id":"t1","status":"done"}"#,
                r#"{"ev":"run.pr","t":"","url":"https://github.com/o/r/pull/7"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(
            wm.join("daemon-queue.jsonl"),
            r#"{"source":"github_issue#12","title":"login broken","run_id":"20261101-aaaa"}"#,
        )
        .unwrap();

        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::ReadOnly,
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        );
        let t = |id: &'static str| trace(ctx.fs.as_ref(), dir.path(), id);
        let by_issue = t("github_issue#12").await;
        assert!(by_issue.contains("run 20261101-aaaa"), "{by_issue}");
        assert!(by_issue.contains("started by: github_issue#12"));
        assert!(by_issue.contains("pr: https://github.com/o/r/pull/7"));
        assert!(by_issue.contains("task t1 [done] patch auth session=sess-42 commits=abcdef1234"));

        // Any neighbour leads back to the same run.
        assert!(t("pull/7").await.contains("20261101-aaaa"));
        assert!(t("sess-42").await.contains("20261101-aaaa"));
        assert!(t("nothing-like-this").await.starts_with("No pilot run"));
    }
}
