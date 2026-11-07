//! Run-status transitions: the source for both `GET /v1/events` and outbound
//! push.
//!
//! Both answer the same question — "what changed across every project since I
//! last looked" — so they share one detector rather than each growing its own
//! idea of what counts as an event. The detector is a poll over the run
//! snapshots on disk: the runs it watches are separate processes, so there is
//! no in-process channel to subscribe to, and `state.json` is rewritten
//! atomically after every event anyway.
//!
//! Push exists so a phone does not have to hold a connection open to learn
//! that a run finished. Delivery is best-effort with one retry: a dead
//! notification endpoint must never be able to stall a run.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use wingman_autonomous::dashboard;
use wingman_autonomous::model::RunStatus;

use super::projects::Project;
use super::ServeState;

/// How often run snapshots are re-read. Runs change on task transitions, not
/// continuously, so this is about how stale a phone's view may be.
pub const POLL: Duration = Duration::from_secs(2);

/// Remembers the last status seen per `(project, run)` so a status that has
/// not changed does not re-fire on every poll.
#[derive(Default)]
pub struct Seen {
    statuses: HashMap<(String, String), RunStatus>,
    /// Runs already reported as `run.died`, so a dead run is said once.
    died: HashSet<(String, String)>,
}

/// One thing worth telling someone about.
#[derive(Debug, Clone)]
pub struct Transition {
    pub event: &'static str,
    pub project: String,
    pub run_id: String,
    pub goal: String,
    pub status: RunStatus,
    /// `run.finished` only: the PR it opened, if any.
    pub pr_url: Option<String>,
    /// `run.finished` only: tasks that ended failed or blocked.
    pub failed_tasks: Vec<String>,
}

impl Transition {
    pub fn to_json(&self) -> Value {
        json!({
            "event": self.event,
            "project": self.project,
            "run_id": self.run_id,
            "goal": self.goal,
            "status": self.status,
            "pr_url": self.pr_url,
            "failed_tasks": self.failed_tasks,
            "text": self.text(),
        })
    }

    /// Human sentence, Slack-incoming-webhook shaped.
    pub fn text(&self) -> String {
        match self.event {
            "run.awaiting_approval" => {
                format!(
                    "Run awaiting plan approval: \"{}\" ({})",
                    self.goal, self.run_id
                )
            }
            "run.finished" => {
                let mut s = format!(
                    "Run {}: \"{}\" ({})",
                    status_word(self.status),
                    self.goal,
                    self.run_id
                );
                if let Some(pr) = &self.pr_url {
                    s.push_str(&format!(" — PR {pr}"));
                }
                if !self.failed_tasks.is_empty() {
                    s.push_str(&format!(
                        " — failed tasks: {}",
                        self.failed_tasks.join(", ")
                    ));
                }
                s
            }
            "run.died" => format!(
                "Run stopped without finishing (last status: {}): \"{}\" ({}). Its process \
                 is gone; see .wingman/autonomous/{}/pilot.log",
                status_word(self.status),
                self.goal,
                self.run_id,
                self.run_id
            ),
            other => format!("{other}: \"{}\" ({})", self.goal, self.run_id),
        }
    }
}

fn status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Done => "finished",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
        RunStatus::AwaitingApproval => "awaiting approval",
        RunStatus::Running => "running",
        RunStatus::Merging => "merging",
        RunStatus::Planning => "planning",
    }
}

/// Poll every project's runs and return the transitions since the last call.
///
/// The first poll after startup records what it finds without emitting: a
/// freshly-started daemon should not push a notification for every run that
/// finished last week.
pub fn poll(projects: &[Project], seen: &mut Seen, emit: bool) -> Vec<Transition> {
    let mut out = Vec::new();
    for project in projects {
        let Ok(runs) = dashboard::list_runs(&project.root) else {
            continue;
        };
        for run in runs {
            let key = (project.id.clone(), run.run_id.clone());
            // A run whose process is gone but whose status never reached a
            // final one: crashed, killed, rejected at the plan gate, or a
            // plan-only run that has nothing more to do.
            if !run.is_terminal() && !seen.died.contains(&key) && process_gone(&run.dir) {
                seen.died.insert(key.clone());
                seen.statuses.insert(key, run.status);
                if emit {
                    out.push(Transition {
                        event: "run.died",
                        project: project.id.clone(),
                        run_id: run.run_id,
                        goal: run.goal,
                        status: run.status,
                        pr_url: None,
                        failed_tasks: Vec::new(),
                    });
                }
                continue;
            }
            let previous = seen.statuses.insert(key, run.status);
            if previous == Some(run.status) || !emit {
                continue;
            }
            let event = match run.status {
                RunStatus::AwaitingApproval => "run.awaiting_approval",
                RunStatus::Done | RunStatus::Failed | RunStatus::Aborted => "run.finished",
                RunStatus::Running if previous.is_none() => continue, // pre-existing run
                RunStatus::Running => "run.started",
                _ => continue,
            };
            let (pr_url, failed_tasks) = if event == "run.finished" {
                finish_details(&run.dir)
            } else {
                (None, Vec::new())
            };
            out.push(Transition {
                event,
                project: project.id.clone(),
                run_id: run.run_id,
                goal: run.goal,
                status: run.status,
                pr_url,
                failed_tasks,
            });
        }
    }
    out
}

/// True when the run recorded a pid and that process no longer exists. A run
/// with no `pid` file (one started before pids were recorded) is never
/// called dead.
// ponytail: a recycled pid reads as alive, so that run goes unreported
// rather than misreported. A start-time check closes it if it ever matters.
fn process_gone(run_dir: &std::path::Path) -> bool {
    std::fs::read_to_string(run_dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|pid| !crate::commands::indexd::process_alive(pid))
}

/// PR url and failed/blocked task ids from a finished run's snapshot.
fn finish_details(run_dir: &std::path::Path) -> (Option<String>, Vec<String>) {
    use wingman_autonomous::model::TaskStatus;
    let Ok(state) = dashboard::load_state(run_dir) else {
        return (None, Vec::new());
    };
    let failed = state
        .tasks
        .iter()
        .filter(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Blocked))
        .map(|t| t.id.clone())
        .collect();
    (state.pr_url, failed)
}

/// One project's run transitions as sentences, for a chat front end (the
/// TUI). Finished, awaiting approval and died only: a run starting is the
/// chat's own doing or noise. Stops when the receiver is dropped.
pub fn watch_project(root: PathBuf) -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let projects = vec![Project {
            id: String::new(),
            root,
        }];
        let mut seen = Seen::default();
        let _ = poll(&projects, &mut seen, false);
        loop {
            tokio::time::sleep(POLL).await;
            for t in poll(&projects, &mut seen, true) {
                if t.event == "run.started" {
                    continue;
                }
                if tx.send(t.text()).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// Background task: watch for transitions and POST the subscribed ones.
///
/// Spawned only when `[serve.push].url` is set. Runs for the life of the
/// daemon.
pub async fn watcher(state: Arc<ServeState>) {
    let Some(url) = state
        .cfg
        .serve
        .push
        .url
        .clone()
        .filter(|u| !u.trim().is_empty())
    else {
        return;
    };
    let wanted = state.cfg.serve.push.events.clone();
    // See `remote.rs`: reqwest needs the process-wide rustls provider.
    wingman_core::ensure_tls_provider();
    let client = reqwest::Client::new();
    let mut seen = Seen::default();
    // Prime without emitting, so startup is quiet.
    let _ = poll(&state.projects, &mut seen, false);

    loop {
        tokio::time::sleep(POLL).await;
        for t in poll(&state.projects, &mut seen, true) {
            if !wanted.is_empty() && !wanted.iter().any(|w| w == t.event) {
                continue;
            }
            let mut payload = t.to_json();
            payload["text"] = json!(t.text());
            deliver(&client, &url, &payload).await;
        }
    }
}

/// POST once, retry once. Anything beyond that is the endpoint's problem, and
/// blocking or queueing here would let a broken webhook become a broken
/// daemon.
async fn deliver(client: &reqwest::Client, url: &str, payload: &Value) {
    for attempt in 0..2 {
        match client.post(url).json(payload).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => tracing::warn!("push to {url} returned {}", resp.status()),
            Err(e) => tracing::warn!("push to {url} failed: {e}"),
        }
        if attempt == 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(root: &std::path::Path, run_id: &str, status: &str) {
        let dir = root.join(".wingman").join("autonomous").join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let state = json!({
            "run_id": run_id,
            "goal": "ship the API",
            "base_commit": "abc",
            "integration_branch": "b",
            "status": status,
            "tasks": [],
        });
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
        std::fs::write(dir.join("tasks.jsonl"), "").unwrap();
    }

    #[test]
    fn a_status_that_has_not_changed_does_not_re_fire() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        seed(&root, "2026-08-18-1042-aaaaaa", "awaiting_approval");
        let projects = vec![Project {
            id: "repo".into(),
            root: root.clone(),
        }];

        let mut seen = Seen::default();
        let first = poll(&projects, &mut seen, true);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].event, "run.awaiting_approval");

        // Same state on the next poll: nothing new to say.
        assert!(poll(&projects, &mut seen, true).is_empty());

        // Now it moves on.
        seed(&root, "2026-08-18-1042-aaaaaa", "done");
        let next = poll(&projects, &mut seen, true);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].event, "run.finished");
    }

    #[test]
    fn finished_runs_carry_their_pr_and_dead_runs_are_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let projects = vec![Project {
            id: "repo".into(),
            root: root.clone(),
        }];
        let mut seen = Seen::default();

        seed(&root, "2026-08-18-1042-dddddd", "running");
        assert!(poll(&projects, &mut seen, true).is_empty());
        let dir = root.join(".wingman/autonomous/2026-08-18-1042-dddddd");
        let mut state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap())
                .unwrap();
        state["status"] = json!("done");
        state["pr_url"] = json!("https://github.com/o/r/pull/9");
        std::fs::write(dir.join("state.json"), state.to_string()).unwrap();
        let done = poll(&projects, &mut seen, true);
        assert_eq!(done[0].event, "run.finished");
        assert!(
            done[0].text().contains("PR https://github.com/o/r/pull/9"),
            "{}",
            done[0].text()
        );

        // A live pid is not dead; a pid that cannot exist is.
        seed(&root, "2026-08-18-1042-eeeeee", "running");
        let live = root.join(".wingman/autonomous/2026-08-18-1042-eeeeee");
        std::fs::write(live.join("pid"), std::process::id().to_string()).unwrap();
        assert!(poll(&projects, &mut seen, true).is_empty());
        std::fs::write(live.join("pid"), u32::MAX.to_string()).unwrap();
        let died = poll(&projects, &mut seen, true);
        assert_eq!(died.len(), 1);
        assert_eq!(died[0].event, "run.died");
        assert!(poll(&projects, &mut seen, true).is_empty(), "said once");
    }

    #[test]
    fn priming_records_without_emitting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        seed(&root, "2026-08-18-1042-bbbbbb", "done");
        let projects = vec![Project {
            id: "repo".into(),
            root,
        }];

        // A daemon that just started must not announce last week's runs.
        let mut seen = Seen::default();
        assert!(poll(&projects, &mut seen, false).is_empty());
        assert!(poll(&projects, &mut seen, true).is_empty());
    }

    #[test]
    fn a_run_already_running_at_startup_is_not_announced_as_started() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        seed(&root, "2026-08-18-1042-cccccc", "running");
        let projects = vec![Project {
            id: "repo".into(),
            root,
        }];
        let mut seen = Seen::default();
        // First sighting of an already-running run: recorded, not announced.
        assert!(poll(&projects, &mut seen, true).is_empty());
    }
}
