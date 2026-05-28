//! Git worktree + integration-merge helpers.
//!
//! Each worker runs in its own worktree under
//! `<project>/.arccode/worktrees/auto-<run-id>-<task-slug>/`, on a branch
//! named `arccode/auto/<run-id>/<task-slug>`. Both are created off the
//! run's `base_commit` so concurrent workers can't trip over each other.
//!
//! When every worker is in `Review`, the orchestrator runs the
//! integration merge: a fresh branch (`arccode/auto/<run-id>`) off
//! `base_commit`, then `git merge --squash <task-branch>` per task in
//! topological order. A merge conflict halts the run and is surfaced via
//! a `run.conflict` event for the user to resolve (or for the
//! `merge-fixer` worker — E4 — to take a swing at).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::model::{RunState, Task, TaskStatus};

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("dep cycle in plan — cannot topo-sort")]
    Cycle,
    #[error("merge conflict on task {task_id}; conflicting files: {}", files.join(", "))]
    Conflict { task_id: String, files: Vec<String> },
    #[error("integration branch {0} already exists on disk")]
    BranchExists(String),
}

impl WorktreeError {
    fn conflict_files(&self) -> Vec<String> {
        match self {
            WorktreeError::Conflict { files, .. } => files.clone(),
            _ => Vec::new(),
        }
    }
}

/// Slug-safe form of a task id for filesystem / branch names. Folds
/// everything outside `[a-z0-9-]` to `-` and trims repeats.
pub fn task_slug(task_id: &str) -> String {
    let mut out = String::with_capacity(task_id.len());
    let mut last_dash = false;
    for c in task_id.chars() {
        let cc = c.to_ascii_lowercase();
        if cc.is_ascii_alphanumeric() {
            out.push(cc);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Branch name for a single task's worktree.
///
/// Note: we use the prefix `arccode/auto-tasks/` rather than the plan.md's
/// nominal `arccode/auto/<run-id>/<task>`. Git treats `/` as a directory
/// boundary in refs, so a branch `arccode/auto/<run-id>` and a branch
/// `arccode/auto/<run-id>/<task>` cannot coexist — the latter occupies a
/// directory the former needs to be a file. Keeping task branches under
/// a sibling prefix preserves the spirit of the plan (task branch ⊂ run
/// namespace) while making the integration merge actually possible.
pub fn task_branch(run_id: &str, task_id: &str) -> String {
    format!("arccode/auto-tasks/{run_id}/{}", task_slug(task_id))
}

/// Create a fresh worktree for `task_id` rooted at `worktree_path`, on a
/// new branch off `base_commit`. The branch name is
/// `arccode/auto/<run-id>/<task-slug>`.
pub fn create_worktree(
    repo_root: &Path,
    base_commit: &str,
    run_id: &str,
    task_id: &str,
    worktree_path: &Path,
) -> Result<String, WorktreeError> {
    let branch = task_branch(run_id, task_id);
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `git worktree add -b <branch> <path> <base>` creates the branch off
    // base_commit and checks it out in the new worktree.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(&branch)
        .arg(worktree_path)
        .arg(base_commit)
        .output()?;
    if !out.status.success() {
        return Err(WorktreeError::Git(format!(
            "worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(branch)
}

/// Remove a worktree (and its branch reference under
/// `.git/worktrees/`). Force-removes so workers that left a dirty tree
/// don't strand the worktree forever.
pub fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> Result<(), WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_path)
        .output()?;
    if !out.status.success() {
        return Err(WorktreeError::Git(format!(
            "worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Belt-and-braces: if the worker left uncommitted changes in the
/// worktree, commit them. Workers are expected to commit themselves, but
/// this guarantees the squash-merge has *something* to merge.
pub fn commit_residual_changes(
    worktree_path: &Path,
    fallback_message: &str,
) -> Result<Option<String>, WorktreeError> {
    let dirty = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("status")
        .arg("--porcelain")
        .output()?;
    if !dirty.status.success() {
        return Err(WorktreeError::Git(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&dirty.stderr).trim()
        )));
    }
    if dirty.stdout.is_empty() {
        return Ok(None);
    }
    let add = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("add")
        .arg("-A")
        .output()?;
    if !add.status.success() {
        return Err(WorktreeError::Git(format!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        )));
    }
    let commit = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("commit")
        .arg("-m")
        .arg(fallback_message)
        // Workers may not have user.email / user.name configured in their
        // worktree; supply env-level defaults so the commit doesn't fail
        // on a vanilla machine.
        .env("GIT_AUTHOR_NAME", "arccode pilot")
        .env("GIT_AUTHOR_EMAIL", "pilot@arccode.local")
        .env("GIT_COMMITTER_NAME", "arccode pilot")
        .env("GIT_COMMITTER_EMAIL", "pilot@arccode.local")
        .output()?;
    if !commit.status.success() {
        return Err(WorktreeError::Git(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        )));
    }
    let sha = rev_parse(worktree_path, "HEAD")?;
    Ok(Some(sha))
}

/// Topologically sort tasks by dep edges. Stable within a layer: tasks
/// with equal depth come out in their original id order. Returns Err on
/// any cycle (caller should already have caught this in
/// [`crate::planner::validate_plan`]).
pub fn topo_sort_tasks(tasks: &[Task]) -> Result<Vec<String>, WorktreeError> {
    // Kahn's algorithm so the order is deterministic.
    let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut indeg: HashMap<String, usize> = tasks
        .iter()
        .map(|t| (t.id.clone(), t.deps.len()))
        .collect();
    let mut ready: VecDeque<String> = tasks
        .iter()
        .filter(|t| t.deps.is_empty())
        .map(|t| t.id.clone())
        .collect();
    // Make iteration order deterministic.
    let mut ready_vec: Vec<String> = ready.drain(..).collect();
    ready_vec.sort();
    for r in &ready_vec {
        ready.push_back(r.clone());
    }

    let mut out = Vec::with_capacity(tasks.len());
    let mut emitted: HashSet<String> = HashSet::new();
    while let Some(id) = ready.pop_front() {
        if !emitted.insert(id.clone()) {
            continue;
        }
        out.push(id.clone());
        // Find tasks that depend on `id` and decrement their indeg.
        let mut newly_ready: Vec<String> = Vec::new();
        for t in tasks {
            if t.deps.iter().any(|d| d == &id) {
                let entry = indeg.entry(t.id.clone()).or_insert(0);
                if *entry > 0 {
                    *entry -= 1;
                    if *entry == 0 {
                        newly_ready.push(t.id.clone());
                    }
                }
            }
        }
        newly_ready.sort();
        for n in newly_ready {
            ready.push_back(n);
        }
    }
    if out.len() != tasks.len() {
        return Err(WorktreeError::Cycle);
    }
    let _ = by_id;
    Ok(out)
}

/// Outcome of [`merge_integration`].
#[derive(Debug, Clone)]
pub struct IntegrationMergeOutcome {
    /// Squash commits, one per task, in merge order.
    pub commits: Vec<TaskMergeCommit>,
}

#[derive(Debug, Clone)]
pub struct TaskMergeCommit {
    pub task_id: String,
    pub commit_sha: String,
}

/// Build the integration branch.
///
/// 1. Create / reset `integration_branch` to point at `base_commit`.
/// 2. Check it out (in the main repo, not a worktree).
/// 3. For each task in topo order: `git merge --squash <task-branch>` +
///    commit. On any conflict, abort with [`WorktreeError::Conflict`].
pub fn merge_integration(
    repo_root: &Path,
    base_commit: &str,
    integration_branch: &str,
    state: &RunState,
) -> Result<IntegrationMergeOutcome, WorktreeError> {
    // 1. Reset integration branch to base.
    let switch = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("switch")
        .arg("-C")
        .arg(integration_branch)
        .arg(base_commit)
        .output()?;
    if !switch.status.success() {
        return Err(WorktreeError::Git(format!(
            "git switch -C {integration_branch} failed: {}",
            String::from_utf8_lossy(&switch.stderr).trim()
        )));
    }

    let order = topo_sort_tasks(&state.tasks)?;
    let mut commits = Vec::with_capacity(order.len());

    for task_id in &order {
        let task = state
            .task(task_id)
            .expect("topo_sort_tasks returned an id not in state");
        // Tasks that aren't ready to merge get skipped — the caller is
        // expected to gate merge_integration on every task being Review.
        if task.status != TaskStatus::Review {
            continue;
        }
        let branch = task_branch(&state.run_id, task_id);
        let squash = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("merge")
            .arg("--squash")
            .arg(&branch)
            .output()?;
        if !squash.status.success() {
            // Detect conflict markers in the index. `git diff --name-only
            // --diff-filter=U` returns the list of unmerged paths.
            let conflicts = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .arg("diff")
                .arg("--name-only")
                .arg("--diff-filter=U")
                .output()?;
            let files: Vec<String> = String::from_utf8_lossy(&conflicts.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // Reset the index so the next attempt isn't poisoned.
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .arg("merge")
                .arg("--abort")
                .output();
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .arg("reset")
                .arg("--hard")
                .output();
            return Err(WorktreeError::Conflict {
                task_id: task_id.clone(),
                files,
            });
        }

        let summary = task
            .outcome
            .as_ref()
            .map(|o| o.summary.as_str())
            .unwrap_or("(no summary)");
        let message = format!("{}\n\n{summary}\n\n[pilot task {}]", task.title, task.id);
        let commit = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("commit")
            .arg("--allow-empty") // squash-merging an already-merged branch
                                  // can produce an empty diff; we still want a
                                  // commit so the run history is one-per-task
            .arg("-m")
            .arg(&message)
            .env("GIT_AUTHOR_NAME", "arccode pilot")
            .env("GIT_AUTHOR_EMAIL", "pilot@arccode.local")
            .env("GIT_COMMITTER_NAME", "arccode pilot")
            .env("GIT_COMMITTER_EMAIL", "pilot@arccode.local")
            .output()?;
        if !commit.status.success() {
            return Err(WorktreeError::Git(format!(
                "git commit (squash) failed for {task_id}: {}",
                String::from_utf8_lossy(&commit.stderr).trim()
            )));
        }
        let sha = rev_parse(repo_root, "HEAD")?;
        commits.push(TaskMergeCommit {
            task_id: task_id.clone(),
            commit_sha: sha,
        });
    }

    let _ = WorktreeError::BranchExists; // keep variant referenced
    let _ = WorktreeError::conflict_files; // keep accessor referenced
    Ok(IntegrationMergeOutcome { commits })
}

/// Remove every per-task worktree under `<project>/.arccode/worktrees/`
/// that matches the run prefix. Best-effort; logs but does not fail.
pub fn cleanup_worktrees(repo_root: &Path, run_id: &str) -> Vec<PathBuf> {
    let dir = repo_root.join(".arccode").join("worktrees");
    let mut removed = Vec::new();
    let prefix = format!("auto-{run_id}-");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        match remove_worktree(repo_root, &path) {
            Ok(()) => removed.push(path),
            Err(e) => tracing::warn!(target: "pilot::worktree", "cleanup_worktrees: {e}"),
        }
    }
    removed
}

fn rev_parse(repo_root: &Path, rev: &str) -> Result<String, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg(rev)
        .output()?;
    if !out.status.success() {
        return Err(WorktreeError::Git(format!(
            "git rev-parse {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, RunState, Task, TaskOutcome};

    fn t(id: &str, deps: Vec<&str>) -> Task {
        let mut t = Task::new(id, Role::Developer, format!("title {id}"));
        t.deps = deps.into_iter().map(String::from).collect();
        t
    }

    #[test]
    fn topo_sort_orders_by_deps_then_id() {
        let tasks = vec![
            t("t3", vec!["t2", "t1"]),
            t("t2", vec!["t1"]),
            t("t1", vec![]),
        ];
        let order = topo_sort_tasks(&tasks).unwrap();
        assert_eq!(order, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn topo_sort_breaks_ties_deterministically() {
        let tasks = vec![
            t("a", vec![]),
            t("c", vec!["a"]),
            t("b", vec!["a"]),
        ];
        let order = topo_sort_tasks(&tasks).unwrap();
        // ties broken by id sort
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_sort_detects_cycle() {
        let tasks = vec![t("a", vec!["b"]), t("b", vec!["a"])];
        match topo_sort_tasks(&tasks) {
            Err(WorktreeError::Cycle) => {}
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn task_slug_canonicalises_ids() {
        assert_eq!(task_slug("t1"), "t1");
        assert_eq!(task_slug("Add Dark Mode!"), "add-dark-mode");
        assert_eq!(task_slug("--foo--bar--"), "foo-bar");
    }

    #[test]
    fn task_branch_avoids_integration_branch_collision() {
        // The integration branch is `arccode/auto/<run-id>`; task branches
        // must live under a sibling namespace or git's directory-style
        // ref tree refuses to create both.
        let run = "2026-05-27-1430-a3f";
        assert_eq!(
            task_branch(run, "t1"),
            "arccode/auto-tasks/2026-05-27-1430-a3f/t1"
        );
        assert!(!task_branch(run, "t1").starts_with(&format!("arccode/auto/{run}")));
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo);
        for a in args {
            cmd.arg(a);
        }
        cmd.env("GIT_AUTHOR_NAME", "arccode pilot")
            .env("GIT_AUTHOR_EMAIL", "pilot@arccode.local")
            .env("GIT_COMMITTER_NAME", "arccode pilot")
            .env("GIT_COMMITTER_EMAIL", "pilot@arccode.local")
            .output()
            .expect("git failed")
    }

    /// Set up a fresh repo with one initial commit; return repo root + the
    /// sha of HEAD. Skip on systems without git in PATH.
    fn init_repo(dir: &Path) -> Option<(PathBuf, String)> {
        if Command::new("git").arg("--version").output().is_err() {
            return None;
        }
        let root = dir.to_path_buf();
        let st = git(&root, &["init", "--initial-branch=main"]);
        if !st.status.success() {
            // Older git: fall back to init then rename.
            git(&root, &["init"]);
        }
        std::fs::write(root.join("seed.txt"), b"hello\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "seed"]);
        let head = String::from_utf8_lossy(&git(&root, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        Some((root, head))
    }

    /// Phase 5 acceptance (plan.md line 675): a clean 3-task run produces
    /// 3 squashed commits on the integration branch and removes all worker
    /// worktrees.
    ///
    /// We can't drive a real worker here without an LLM, so we simulate
    /// the worker manually: create the worktree, write+commit a file in
    /// it, then run the integration merge. The end state must contain
    /// exactly 3 squash commits on `arccode/auto/<run-id>` and 0 worker
    /// worktrees under .arccode/worktrees/.
    #[test]
    fn three_task_run_produces_three_squashed_commits_and_cleans_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };

        let run_id = "test-run-5";
        let tasks = vec![
            ("t1", vec![], "file-t1.txt", "hello t1"),
            ("t2", vec!["t1"], "file-t2.txt", "hello t2"),
            ("t3", vec!["t1", "t2"], "file-t3.txt", "hello t3"),
        ];

        // Build state with each task pre-marked Review so merge_integration
        // will process it.
        let mut state = RunState::new(run_id, "demo goal", &base, "arccode/auto/test-run-5");
        for (id, deps, _, _) in &tasks {
            let mut task = Task::new(*id, Role::Developer, format!("Add {id}"));
            task.deps = deps.iter().map(|s| s.to_string()).collect();
            task.status = TaskStatus::Review;
            task.outcome = Some(TaskOutcome {
                summary: format!("created {id}.txt"),
                files_changed: vec![format!("file-{id}.txt")],
            });
            state.tasks.push(task);
        }

        // Simulate workers: each creates a worktree, writes its file,
        // commits, and leaves the branch in place for integration.
        for (id, _, filename, body) in &tasks {
            let wt_path = repo
                .join(".arccode")
                .join("worktrees")
                .join(format!("auto-{run_id}-{}", id));
            let _branch = create_worktree(&repo, &base, run_id, id, &wt_path).unwrap();
            std::fs::write(wt_path.join(filename), body.as_bytes()).unwrap();
            git(&wt_path, &["add", "-A"]);
            git(&wt_path, &["commit", "-m", &format!("add {filename}")]);
        }

        // Integration merge.
        let outcome = merge_integration(&repo, &base, "arccode/auto/test-run-5", &state)
            .expect("integration merge succeeded");
        assert_eq!(outcome.commits.len(), 3);
        let ids: Vec<&str> = outcome.commits.iter().map(|c| c.task_id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"], "merge order must respect deps");

        // Verify 3 commits on top of base.
        let log = git(
            &repo,
            &["log", "--format=%s", &format!("{base}..arccode/auto/test-run-5")],
        );
        let subjects: Vec<&str> = std::str::from_utf8(&log.stdout)
            .unwrap()
            .lines()
            .collect();
        assert_eq!(subjects.len(), 3, "expected 3 squash commits; got {subjects:?}");
        // Newest-first order: t3, t2, t1.
        assert!(subjects[0].starts_with("Add t3"));
        assert!(subjects[1].starts_with("Add t2"));
        assert!(subjects[2].starts_with("Add t1"));

        // Verify the merged tree contains all three files.
        for (id, _, filename, _) in &tasks {
            let path = repo.join(filename);
            assert!(path.exists(), "{filename} missing after merge for {id}");
        }

        // Cleanup worktrees and confirm they're gone.
        let removed = cleanup_worktrees(&repo, run_id);
        assert_eq!(removed.len(), 3, "expected 3 worktrees removed");
        let leftovers: Vec<_> = std::fs::read_dir(repo.join(".arccode").join("worktrees"))
            .map(|r| {
                r.filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                    .filter(|n| n.to_string_lossy().starts_with(&format!("auto-{run_id}-")))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "expected zero worker worktrees after cleanup, found {leftovers:?}"
        );
    }

    /// Conflict path: two tasks edit the same file. merge_integration must
    /// surface a WorktreeError::Conflict with the offending file listed.
    #[test]
    fn conflicting_writes_surface_conflict_error() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };

        let run_id = "conflict-test";
        let tasks: Vec<(&str, Vec<&str>, &str)> =
            vec![("t1", vec![], "hello A"), ("t2", vec![], "hello B")];
        let mut state = RunState::new(run_id, "demo", &base, "arccode/auto/conflict-test");
        for (id, deps, _) in &tasks {
            let mut task = Task::new(*id, Role::Developer, format!("Edit shared {id}"));
            task.deps = deps.iter().map(|s| s.to_string()).collect();
            task.status = TaskStatus::Review;
            task.outcome = Some(TaskOutcome {
                summary: "edited shared".into(),
                files_changed: vec!["shared.txt".into()],
            });
            state.tasks.push(task);
        }

        // Each task creates a worktree off `base` and writes a DIFFERENT
        // body to the same file. squash-merging both into a clean branch
        // must conflict on the second.
        for (id, _, body) in &tasks {
            let wt_path = repo
                .join(".arccode")
                .join("worktrees")
                .join(format!("auto-{run_id}-{}", id));
            create_worktree(&repo, &base, run_id, id, &wt_path).unwrap();
            std::fs::write(wt_path.join("shared.txt"), body.as_bytes()).unwrap();
            git(&wt_path, &["add", "-A"]);
            git(&wt_path, &["commit", "-m", &format!("touch shared from {id}")]);
        }

        match merge_integration(&repo, &base, "arccode/auto/conflict-test", &state) {
            Err(WorktreeError::Conflict { task_id, files }) => {
                assert_eq!(task_id, "t2", "second task should be the one that conflicts");
                assert!(
                    files.iter().any(|f| f.ends_with("shared.txt")),
                    "expected shared.txt in conflict list, got {files:?}"
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Clean up so test artifacts don't linger.
        let _ = cleanup_worktrees(&repo, run_id);
    }
}
