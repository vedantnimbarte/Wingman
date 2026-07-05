//! E4 — conflict avoidance via write-set scheduling.
//!
//! The M1 strategy was "linearize merges at the end, halt on first
//! conflict." This module replaces the *runtime* half of E4: never run two
//! tasks whose declared `writes` globs overlap concurrently, so most merge
//! conflicts are designed out before they can happen. (Rebase-as-you-go +
//! the `merge-fixer` worker handle the residual; the planner's critique
//! pass already catches static overlap inside a wave.)
//!
//! Glob overlap is undecidable in general; we use a sound-enough
//! approximation built on three cheap checks (see [`paths_overlap`]):
//! exact equality, glob-matches-literal either direction, and shared
//! literal directory prefix. It errs toward declaring overlap (serialising
//! a pair that might be independent) rather than missing a real conflict —
//! the safe direction.

use std::collections::HashMap;

use globset::Glob;

use crate::model::Task;

/// Characters that begin a glob wildcard.
fn first_wildcard(p: &str) -> Option<usize> {
    p.find(['*', '?', '[', '{'])
}

/// The literal path prefix of `p` up to (not including) the first
/// wildcard, trimmed to whole path components. For `crates/x/**` →
/// `crates/x`; for a literal path → the path itself.
fn literal_prefix(p: &str) -> &str {
    match first_wildcard(p) {
        None => p.trim_end_matches('/'),
        Some(i) => {
            let head = &p[..i];
            // Trim back to the last complete component.
            match head.rfind('/') {
                Some(slash) => &head[..slash],
                None => "",
            }
        }
    }
}

/// True when `a`'s components are a path-prefix of `b`'s (or vice versa) —
/// i.e. one directory contains the other.
fn is_path_prefix(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        // An empty literal prefix means "matches anywhere under root";
        // treat as overlapping to stay safe.
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let sc: Vec<&str> = short.split('/').collect();
    let lc: Vec<&str> = long.split('/').collect();
    sc.iter().zip(lc.iter()).all(|(x, y)| x == y) && sc.len() <= lc.len()
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(candidate))
        .unwrap_or(false)
}

/// Do two individual write paths/globs overlap?
pub fn paths_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // One literal, one glob (or both literal): direct glob match.
    if glob_matches(a, b) || glob_matches(b, a) {
        return true;
    }
    // Both may be globs: compare literal directory prefixes.
    is_path_prefix(literal_prefix(a), literal_prefix(b))
}

/// Do two write-sets overlap on any pair of entries?
pub fn writes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|pa| b.iter().any(|pb| paths_overlap(pa, pb)))
}

/// Two tasks conflict if their write-sets overlap. Tasks with no declared
/// writes are treated as conflicting with everything (we can't prove they
/// don't touch a shared file), forcing them to run alone — this nudges the
/// planner to declare `writes` (E3).
pub fn tasks_conflict(a: &Task, b: &Task) -> bool {
    if a.writes.is_empty() || b.writes.is_empty() {
        return true;
    }
    writes_overlap(&a.writes, &b.writes)
}

/// Pick the next wave of tasks to dispatch: a maximal subset of
/// `eligible` (deps already met, status todo) that conflicts neither with
/// any currently-`running` task nor with each other, capped at
/// `available_slots`.
///
/// Greedy in input order, so callers control priority by ordering
/// `eligible` (e.g. by dependency depth then id). Returns the chosen
/// tasks; the caller spawns workers for them.
pub fn select_wave<'a>(
    eligible: &[&'a Task],
    running: &[&Task],
    available_slots: usize,
) -> Vec<&'a Task> {
    let mut chosen: Vec<&'a Task> = Vec::new();
    for &cand in eligible {
        if chosen.len() >= available_slots {
            break;
        }
        let conflicts_running = running.iter().any(|r| tasks_conflict(cand, r));
        let conflicts_chosen = chosen.iter().any(|c| tasks_conflict(cand, c));
        if !conflicts_running && !conflicts_chosen {
            chosen.push(cand);
        }
    }
    chosen
}

/// A dependency-graph validation failure. Shared by the planner's static
/// check and the runtime task mutators (`add_task`, the E5 splitter) so a
/// task added mid-run is held to the same acyclic/known-dep invariant the
/// initial plan is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DagError {
    /// A task lists itself as one of its own deps.
    SelfDep(String),
    /// A dep references an id no task in the graph carries.
    UnknownDep { task: String, dep: String },
    /// The deps form a cycle — no topological order exists.
    Cycle,
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::SelfDep(t) => write!(f, "task {t} depends on itself"),
            DagError::UnknownDep { task, dep } => {
                write!(f, "task {task} depends on unknown id {dep}")
            }
            DagError::Cycle => write!(f, "dependency cycle"),
        }
    }
}

impl std::error::Error for DagError {}

/// True when the `id → deps` adjacency map contains a cycle. Three-colour
/// DFS: white = unseen, gray = on the current stack (a gray hit is a back
/// edge → cycle), black = fully explored. Deps pointing at ids absent from
/// the map are ignored here (that's [`validate_edges`]'s job), so this is
/// safe to call on partial graphs.
pub fn edges_have_cycle(edges: &HashMap<String, Vec<String>>) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        White,
        Gray,
        Black,
    }
    fn visit<'a>(
        node: &'a str,
        marks: &mut HashMap<&'a str, Mark>,
        edges: &'a HashMap<String, Vec<String>>,
    ) -> bool {
        match marks.get(node).copied().unwrap_or(Mark::White) {
            Mark::Gray => return true,
            Mark::Black => return false,
            Mark::White => {}
        }
        marks.insert(node, Mark::Gray);
        if let Some(deps) = edges.get(node) {
            for d in deps {
                if visit(d, marks, edges) {
                    return true;
                }
            }
        }
        marks.insert(node, Mark::Black);
        false
    }

    let mut marks: HashMap<&str, Mark> = HashMap::with_capacity(edges.len());
    for id in edges.keys() {
        if visit(id, &mut marks, edges) {
            return true;
        }
    }
    false
}

/// Validate an `id → deps` adjacency map: no task may depend on itself,
/// every dep must reference a known id, and the graph must be acyclic.
///
/// The planner runs this over the whole plan before persisting; the
/// orchestrator runs it over the *projected* graph (current tasks plus the
/// pending mutation) before appending a `task.create`, so a manager- or
/// splitter-issued edge can never wedge the run with a cycle or a dangling
/// dep the scheduler would wait on forever.
pub fn validate_edges(edges: &HashMap<String, Vec<String>>) -> Result<(), DagError> {
    for (id, deps) in edges {
        for d in deps {
            if d == id {
                return Err(DagError::SelfDep(id.clone()));
            }
            if !edges.contains_key(d) {
                return Err(DagError::UnknownDep {
                    task: id.clone(),
                    dep: d.clone(),
                });
            }
        }
    }
    if edges_have_cycle(edges) {
        return Err(DagError::Cycle);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, Task};

    fn task(id: &str, writes: &[&str]) -> Task {
        let mut t = Task::new(id, Role::Developer, id);
        t.writes = writes.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn identical_paths_overlap() {
        assert!(paths_overlap("a/b.rs", "a/b.rs"));
    }

    #[test]
    fn glob_matches_literal() {
        assert!(paths_overlap("crates/**/*.rs", "crates/x/src/main.rs"));
        assert!(paths_overlap("crates/x/src/main.rs", "crates/**/*.rs"));
    }

    #[test]
    fn glob_vs_glob_shared_dir_overlaps() {
        assert!(paths_overlap("crates/cli/**", "crates/cli/src/*.rs"));
    }

    #[test]
    fn disjoint_dirs_do_not_overlap() {
        assert!(!paths_overlap("crates/cli/**", "crates/core/**"));
        assert!(!paths_overlap("crates/cli/a.rs", "crates/cli/b.rs"));
    }

    #[test]
    fn writes_overlap_detects_any_pair() {
        let a = vec!["crates/cli/main.rs".to_string(), "docs/x.md".to_string()];
        let b = vec!["README.md".to_string(), "docs/x.md".to_string()];
        assert!(writes_overlap(&a, &b));
        let c = vec!["crates/core/lib.rs".to_string()];
        assert!(!writes_overlap(&a, &c));
    }

    #[test]
    fn empty_writes_conflict_with_everything() {
        let a = task("t1", &[]);
        let b = task("t2", &["crates/x/a.rs"]);
        assert!(tasks_conflict(&a, &b));
    }

    #[test]
    fn select_wave_serialises_conflicting_tasks() {
        let t1 = task("t1", &["crates/cli/main.rs"]);
        let t2 = task("t2", &["crates/cli/main.rs"]); // conflicts with t1
        let t3 = task("t3", &["crates/core/lib.rs"]); // independent
        let eligible = vec![&t1, &t2, &t3];
        let wave = select_wave(&eligible, &[], 4);
        let ids: Vec<&str> = wave.iter().map(|t| t.id.as_str()).collect();
        // t1 and t3 can run; t2 is held back (conflicts with t1).
        assert_eq!(ids, vec!["t1", "t3"]);
    }

    #[test]
    fn select_wave_respects_running_tasks() {
        let running = task("r1", &["crates/cli/main.rs"]);
        let t1 = task("t1", &["crates/cli/main.rs"]); // conflicts with running
        let t2 = task("t2", &["crates/core/lib.rs"]); // free
        let eligible = vec![&t1, &t2];
        let wave = select_wave(&eligible, &[&running], 4);
        let ids: Vec<&str> = wave.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["t2"]);
    }

    #[test]
    fn select_wave_honors_slot_cap() {
        let t1 = task("t1", &["a/1.rs"]);
        let t2 = task("t2", &["b/2.rs"]);
        let t3 = task("t3", &["c/3.rs"]);
        let eligible = vec![&t1, &t2, &t3];
        let wave = select_wave(&eligible, &[], 2);
        assert_eq!(wave.len(), 2);
    }

    #[test]
    fn select_wave_empty_when_no_slots() {
        let t1 = task("t1", &["a/1.rs"]);
        assert!(select_wave(&[&t1], &[], 0).is_empty());
    }

    fn edges(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(id, deps)| {
                (
                    id.to_string(),
                    deps.iter().map(|s| s.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn validate_edges_accepts_a_clean_dag() {
        let g = edges(&[("t1", &[]), ("t2", &["t1"]), ("t3", &["t1", "t2"])]);
        assert!(validate_edges(&g).is_ok());
    }

    #[test]
    fn validate_edges_rejects_self_dep() {
        let g = edges(&[("t1", &["t1"])]);
        assert_eq!(validate_edges(&g), Err(DagError::SelfDep("t1".into())));
    }

    #[test]
    fn validate_edges_rejects_unknown_dep() {
        let g = edges(&[("t1", &["t99"])]);
        assert_eq!(
            validate_edges(&g),
            Err(DagError::UnknownDep {
                task: "t1".into(),
                dep: "t99".into()
            })
        );
    }

    #[test]
    fn validate_edges_rejects_direct_cycle() {
        let g = edges(&[("t1", &["t2"]), ("t2", &["t1"])]);
        assert_eq!(validate_edges(&g), Err(DagError::Cycle));
    }

    #[test]
    fn validate_edges_rejects_indirect_cycle() {
        let g = edges(&[("t1", &["t2"]), ("t2", &["t3"]), ("t3", &["t1"])]);
        assert_eq!(validate_edges(&g), Err(DagError::Cycle));
        assert!(edges_have_cycle(&g));
    }
}
