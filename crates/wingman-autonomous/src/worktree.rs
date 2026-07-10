//! Git worktree + integration-merge helpers.
//!
//! Each worker runs in its own worktree under
//! `<project>/.wingman/worktrees/auto-<run-id>-<task-slug>/`, on a branch
//! named `wingman/auto/<run-id>/<task-slug>`. Both are created off the
//! run's `base_commit` so concurrent workers can't trip over each other.
//!
//! When every worker is in `Review`, the orchestrator runs the
//! integration merge: a fresh branch (`wingman/auto/<run-id>`) off
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
/// Note: we use the prefix `wingman/auto-tasks/` rather than the plan.md's
/// nominal `wingman/auto/<run-id>/<task>`. Git treats `/` as a directory
/// boundary in refs, so a branch `wingman/auto/<run-id>` and a branch
/// `wingman/auto/<run-id>/<task>` cannot coexist — the latter occupies a
/// directory the former needs to be a file. Keeping task branches under
/// a sibling prefix preserves the spirit of the plan (task branch ⊂ run
/// namespace) while making the integration merge actually possible.
pub fn task_branch(run_id: &str, task_id: &str) -> String {
    format!("wingman/auto-tasks/{run_id}/{}", task_slug(task_id))
}

/// The diff a task's branch adds on top of the run's base commit. The E7
/// inline reviewer needs this to review the actual change instead of the
/// worker's self-description. Best-effort: a missing branch, a git error, or
/// an empty diff all yield `None` (the caller then approves rather than
/// rejecting a change it cannot see).
pub fn task_diff(
    repo_root: &Path,
    run_id: &str,
    task_id: &str,
    base_commit: &str,
) -> Option<String> {
    let branch = task_branch(run_id, task_id);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("diff")
        .arg(format!("{base_commit}..{branch}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let diff = String::from_utf8_lossy(&out.stdout);
    let diff = diff.trim();
    if diff.is_empty() {
        None
    } else {
        Some(diff.to_string())
    }
}

/// Create a fresh worktree for `task_id` rooted at `worktree_path`, on a
/// new branch off `base_commit`. The branch name is
/// `wingman/auto/<run-id>/<task-slug>`.
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
    // Make creation idempotent. A crash / Ctrl+C / prior failed attempt can
    // leave this worktree directory and branch on disk; `git worktree add -b`
    // would then fail with "already exists" and the task could never be
    // reassigned (this is exactly what wedged `pilot resume`). Clear stale
    // state first — all best-effort no-ops when nothing is left over.
    prune_stale_worktree(repo_root, worktree_path, &branch);
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

/// Clear any leftover worktree directory and branch from a prior attempt so
/// `create_worktree` can recreate them. Every step is best-effort: `prune`
/// does nothing when there are no stale admin entries, the remove/rm are
/// skipped when the path is absent, and `branch -D` fails harmlessly when the
/// branch doesn't exist.
fn prune_stale_worktree(repo_root: &Path, worktree_path: &Path, branch: &str) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "prune"])
        .output();
    if worktree_path.exists() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .output();
        // If git didn't own the path (e.g. it was pruned but the dir stayed),
        // make sure it's gone so `worktree add` doesn't refuse.
        let _ = std::fs::remove_dir_all(worktree_path);
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["branch", "-D", branch])
        .output();
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
        .env("GIT_AUTHOR_NAME", "wingman pilot")
        .env("GIT_AUTHOR_EMAIL", "pilot@wingman.local")
        .env("GIT_COMMITTER_NAME", "wingman pilot")
        .env("GIT_COMMITTER_EMAIL", "pilot@wingman.local")
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
    let mut indeg: HashMap<String, usize> =
        tasks.iter().map(|t| (t.id.clone(), t.deps.len())).collect();
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

/// E4 — rebase `branch` onto `onto`, then restore HEAD to `onto` regardless
/// of outcome so the caller can carry on merging from a known checkout.
/// Returns `Ok` when the rebase applied cleanly, `Err` when it conflicted
/// (in which case the rebase is aborted and `branch` is left untouched).
///
/// The HEAD-restoration postcondition is the important invariant: `git
/// rebase <onto> <branch>` checks out `branch`, so without the final switch
/// the merge loop would be on the wrong branch for the squash.
fn rebase_branch_onto(repo_root: &Path, branch: &str, onto: &str) -> Result<(), WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rebase")
        .arg(onto)
        .arg(branch)
        .output()?;
    let ok = out.status.success();
    if !ok {
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("rebase")
            .arg("--abort")
            .output();
    }
    // Restore HEAD to the integration branch either way.
    let switch = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("switch")
        .arg(onto)
        .output()?;
    if !switch.status.success() {
        return Err(WorktreeError::Git(format!(
            "git switch {onto} after rebase failed: {}",
            String::from_utf8_lossy(&switch.stderr).trim()
        )));
    }
    if ok {
        Ok(())
    } else {
        Err(WorktreeError::Git(format!(
            "rebase of {branch} onto {onto} conflicted"
        )))
    }
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
        // Merge every successfully completed task. A task lands in Done when
        // the manager finalizes it incrementally, or is left in Review for
        // this end-of-run merge — either way its branch holds work to
        // integrate. Anything else (Failed / still Pending / InProgress) has
        // no mergeable branch and is skipped.
        if !matches!(task.status, TaskStatus::Review | TaskStatus::Done) {
            continue;
        }
        let branch = task_branch(&state.run_id, task_id);
        // E4 rebase-as-you-go: replay this task's branch onto the current
        // integration tip before squashing, so its diff is computed against
        // the work already merged rather than the run's base commit. This
        // designs out spurious conflicts from base-relative diffs. A rebase
        // that genuinely conflicts is aborted (HEAD restored to the
        // integration branch) and we fall through to the squash path, which
        // surfaces the real conflict exactly as before. Best-effort.
        let _ = rebase_branch_onto(repo_root, &branch, integration_branch);
        let squash = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("merge")
            .arg("--squash")
            .arg(&branch)
            .output()?;
        if !squash.status.success() {
            // Capture stderr up-front so a non-conflict failure (e.g. a
            // refusal to merge unrelated histories, or a worktree lock)
            // surfaces a useful diagnostic instead of being misreported
            // as an empty Conflict.
            let squash_stderr = String::from_utf8_lossy(&squash.stderr).trim().to_string();
            // Detect conflict markers in the index. `git diff --name-only
            // --diff-filter=U` returns the list of unmerged paths. On
            // Windows we've seen the squash exit non-zero before the
            // index is populated with conflict entries, so also fall back
            // to scraping `CONFLICT (…): … <path>` lines out of stderr.
            let conflicts = Command::new("git")
                .arg("-C")
                .arg(repo_root)
                .arg("diff")
                .arg("--name-only")
                .arg("--diff-filter=U")
                .output()?;
            let mut files: Vec<String> = String::from_utf8_lossy(&conflicts.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if files.is_empty() {
                for line in squash_stderr.lines() {
                    if let Some(rest) = line.strip_prefix("CONFLICT") {
                        if let Some(idx) = rest.rfind(' ') {
                            let candidate = rest[idx + 1..].trim().to_string();
                            if !candidate.is_empty() && !files.contains(&candidate) {
                                files.push(candidate);
                            }
                        }
                    }
                }
            }
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
            if files.is_empty() {
                // Not a content conflict — return the raw git error so the
                // caller can see what actually broke (e.g. lock contention,
                // unrelated histories, missing ref).
                return Err(WorktreeError::Git(format!(
                    "git merge --squash {branch} failed for {task_id} with no conflict files: {squash_stderr}"
                )));
            }
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
            .env("GIT_AUTHOR_NAME", "wingman pilot")
            .env("GIT_AUTHOR_EMAIL", "pilot@wingman.local")
            .env("GIT_COMMITTER_NAME", "wingman pilot")
            .env("GIT_COMMITTER_EMAIL", "pilot@wingman.local")
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

/// Remove every per-task worktree under `<project>/.wingman/worktrees/`
/// that matches the run prefix. Best-effort; logs but does not fail.
pub fn cleanup_worktrees(repo_root: &Path, run_id: &str) -> Vec<PathBuf> {
    let dir = repo_root.join(".wingman").join("worktrees");
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

/// Delete every per-task branch for a run (`wingman/auto-tasks/<run>/…`).
/// After a successful integration merge these are squashed into the
/// integration branch and no longer referenced, so keeping them just leaks
/// refs that accumulate across every run. Best-effort; returns how many were
/// deleted and never fails.
pub fn cleanup_task_branches(repo_root: &Path, run_id: &str) -> usize {
    let pattern = format!("refs/heads/wingman/auto-tasks/{run_id}/");
    let Ok(list) = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["for-each-ref", "--format=%(refname:short)"])
        .arg(&pattern)
        .output()
    else {
        return 0;
    };
    let mut deleted = 0;
    for branch in String::from_utf8_lossy(&list.stdout).lines() {
        let branch = branch.trim();
        if branch.is_empty() {
            continue;
        }
        let ok = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["branch", "-D", branch])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            deleted += 1;
        }
    }
    deleted
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
        let tasks = vec![t("a", vec![]), t("c", vec!["a"]), t("b", vec!["a"])];
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
        // The integration branch is `wingman/auto/<run-id>`; task branches
        // must live under a sibling namespace or git's directory-style
        // ref tree refuses to create both.
        let run = "2026-05-27-1430-a3f";
        assert_eq!(
            task_branch(run, "t1"),
            "wingman/auto-tasks/2026-05-27-1430-a3f/t1"
        );
        assert!(!task_branch(run, "t1").starts_with(&format!("wingman/auto/{run}")));
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo);
        for a in args {
            cmd.arg(a);
        }
        cmd.env("GIT_AUTHOR_NAME", "wingman pilot")
            .env("GIT_AUTHOR_EMAIL", "pilot@wingman.local")
            .env("GIT_COMMITTER_NAME", "wingman pilot")
            .env("GIT_COMMITTER_EMAIL", "pilot@wingman.local")
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
        // Neutralise Windows defaults that bleed in from the global
        // config (GH Actions Windows runners ship with autocrlf=true).
        // Without this, the squash-merge tests can hit spurious
        // line-ending differences that read as conflicts.
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "core.eol", "lf"]);
        // CI runners (GH Actions ubuntu-latest / windows-latest) have no
        // global `user.email` / `user.name`. The test helper passes
        // GIT_AUTHOR_* / GIT_COMMITTER_* env vars for its own invocations,
        // but production code in `merge_integration` shells out to git
        // without those env vars and would otherwise hit "Committer
        // identity unknown". Persisting identity in the repo's local
        // config picks up for every git process spawned against this repo.
        git(&root, &["config", "user.email", "pilot@wingman.local"]);
        git(&root, &["config", "user.name", "wingman pilot"]);
        std::fs::write(root.join("seed.txt"), b"hello\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-m", "seed"]);
        let head = String::from_utf8_lossy(&git(&root, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        Some((root, head))
    }

    /// E4 rebase-as-you-go: `rebase_branch_onto` replays a non-conflicting
    /// branch onto the integration tip and restores HEAD to it; a conflicting
    /// branch aborts cleanly and still restores HEAD.
    #[test]
    fn e4_rebase_branch_onto_restores_head_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, _base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };
        // integration branch adds a.txt on top of seed.
        git(&repo, &["switch", "-c", "integration"]);
        std::fs::write(repo.join("a.txt"), "from integration").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "int: a"]);

        // A non-conflicting feature branch off the seed touches b.txt.
        git(&repo, &["switch", "-c", "feat-clean", "main"]);
        std::fs::write(repo.join("b.txt"), "from feat").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "feat: b"]);
        git(&repo, &["switch", "integration"]);

        rebase_branch_onto(&repo, "feat-clean", "integration").expect("clean rebase");
        let head = String::from_utf8_lossy(&git(&repo, &["symbolic-ref", "--short", "HEAD"]).stdout)
            .trim()
            .to_string();
        assert_eq!(head, "integration", "HEAD must be restored after clean rebase");

        // A conflicting branch also edits a.txt.
        git(&repo, &["switch", "-c", "feat-conflict", "main"]);
        std::fs::write(repo.join("a.txt"), "from feat conflict").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "feat: a-conflict"]);
        git(&repo, &["switch", "integration"]);

        assert!(rebase_branch_onto(&repo, "feat-conflict", "integration").is_err());
        let head = String::from_utf8_lossy(&git(&repo, &["symbolic-ref", "--short", "HEAD"]).stdout)
            .trim()
            .to_string();
        assert_eq!(head, "integration", "HEAD must be restored after aborted rebase");
        // No rebase in progress left dangling.
        assert!(!repo.join(".git").join("rebase-merge").exists());
    }

    /// Resume-safety: re-creating a worktree for a task whose prior attempt
    /// left the worktree dir + branch on disk must succeed, not fail with
    /// "already exists". This is the case `pilot resume` used to wedge on.
    #[test]
    fn create_worktree_is_idempotent_over_stale_state() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };
        let wt = repo.join(".wingman/worktrees/t1");
        let run_id = "2026-01-01-0000-abcdef";

        // First attempt creates the worktree + branch and leaves a dirty file.
        let branch = create_worktree(&repo, &base, run_id, "t1", &wt).expect("first create");
        std::fs::write(wt.join("scratch.txt"), "partial work").unwrap();
        assert!(wt.exists());

        // Simulate an interrupted run: the dir and branch are still present.
        // A naive `git worktree add -b` would now fail; create_worktree must
        // prune the stale state and recreate cleanly.
        let branch2 = create_worktree(&repo, &base, run_id, "t1", &wt).expect("recreate over stale");
        assert_eq!(branch, branch2);
        assert!(wt.exists());
    }

    /// Phase 5 acceptance (plan.md line 675): a clean 3-task run produces
    /// 3 squashed commits on the integration branch and removes all worker
    /// worktrees.
    ///
    /// We can't drive a real worker here without an LLM, so we simulate
    /// the worker manually: create the worktree, write+commit a file in
    /// it, then run the integration merge. The end state must contain
    /// exactly 3 squash commits on `wingman/auto/<run-id>` and 0 worker
    /// worktrees under .wingman/worktrees/.
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
        let mut state = RunState::new(run_id, "demo goal", &base, "wingman/auto/test-run-5");
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
                .join(".wingman")
                .join("worktrees")
                .join(format!("auto-{run_id}-{}", id));
            let _branch = create_worktree(&repo, &base, run_id, id, &wt_path).unwrap();
            std::fs::write(wt_path.join(filename), body.as_bytes()).unwrap();
            git(&wt_path, &["add", "-A"]);
            git(&wt_path, &["commit", "-m", &format!("add {filename}")]);
        }

        // Integration merge.
        let outcome = merge_integration(&repo, &base, "wingman/auto/test-run-5", &state)
            .expect("integration merge succeeded");
        assert_eq!(outcome.commits.len(), 3);
        let ids: Vec<&str> = outcome.commits.iter().map(|c| c.task_id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"], "merge order must respect deps");

        // Verify 3 commits on top of base.
        let log = git(
            &repo,
            &[
                "log",
                "--format=%s",
                &format!("{base}..wingman/auto/test-run-5"),
            ],
        );
        let subjects: Vec<&str> = std::str::from_utf8(&log.stdout).unwrap().lines().collect();
        assert_eq!(
            subjects.len(),
            3,
            "expected 3 squash commits; got {subjects:?}"
        );
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
        let leftovers: Vec<_> = std::fs::read_dir(repo.join(".wingman").join("worktrees"))
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

    /// Regression for the incremental-finalize merge gap: a task the manager
    /// finalized to Done (not left in Review) must still be squash-merged, or
    /// its committed work never reaches the integration branch and cleanup
    /// then deletes the only copy.
    #[test]
    fn done_status_tasks_are_merged_not_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };
        let run_id = "test-run-done";
        let branch = "wingman/auto/test-run-done";

        let mut state = RunState::new(run_id, "demo goal", &base, branch);
        let mut task = Task::new("t1", Role::Developer, "Add t1");
        task.status = TaskStatus::Done; // finalized incrementally, not Review
        state.tasks.push(task);

        let wt_path = repo
            .join(".wingman")
            .join("worktrees")
            .join(format!("auto-{run_id}-t1"));
        create_worktree(&repo, &base, run_id, "t1", &wt_path).unwrap();
        std::fs::write(wt_path.join("file-t1.txt"), b"hello t1").unwrap();
        git(&wt_path, &["add", "-A"]);
        git(&wt_path, &["commit", "-m", "add file-t1.txt"]);

        let outcome = merge_integration(&repo, &base, branch, &state)
            .expect("integration merge succeeded");
        assert_eq!(outcome.commits.len(), 1, "the Done task should be merged");
        assert!(
            repo.join("file-t1.txt").exists(),
            "Done task's file must land on the integration branch"
        );
    }

    /// The E7 reviewer feeds on this: task_diff must return the branch's
    /// change against base, and None when there's nothing (so the reviewer
    /// approves rather than rejecting a change it can't see).
    #[test]
    fn task_diff_returns_branch_changes_or_none() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((repo, base)) = init_repo(tmp.path()) else {
            eprintln!("skipping: git not available");
            return;
        };
        let run_id = "test-run-diff";
        let wt_path = repo
            .join(".wingman")
            .join("worktrees")
            .join(format!("auto-{run_id}-t1"));
        create_worktree(&repo, &base, run_id, "t1", &wt_path).unwrap();
        std::fs::write(wt_path.join("new.txt"), b"added line\n").unwrap();
        git(&wt_path, &["add", "-A"]);
        git(&wt_path, &["commit", "-m", "add new.txt"]);

        let diff = task_diff(&repo, run_id, "t1", &base).expect("diff present");
        assert!(diff.contains("new.txt") && diff.contains("added line"));
        // A task with no branch → None (reviewer then approves, not rejects).
        assert!(task_diff(&repo, run_id, "nonexistent", &base).is_none());
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
        let mut state = RunState::new(run_id, "demo", &base, "wingman/auto/conflict-test");
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
                .join(".wingman")
                .join("worktrees")
                .join(format!("auto-{run_id}-{}", id));
            create_worktree(&repo, &base, run_id, id, &wt_path).unwrap();
            std::fs::write(wt_path.join("shared.txt"), body.as_bytes()).unwrap();
            git(&wt_path, &["add", "-A"]);
            git(
                &wt_path,
                &["commit", "-m", &format!("touch shared from {id}")],
            );
        }

        match merge_integration(&repo, &base, "wingman/auto/conflict-test", &state) {
            Err(WorktreeError::Conflict { task_id, files }) => {
                assert_eq!(
                    task_id, "t2",
                    "second task should be the one that conflicts"
                );
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
