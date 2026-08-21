//! Dispatching a card into a pilot run.
//!
//! `board dispatch` spawns `wingman pilot run "<goal>" --detached` with the
//! card's project as cwd, then records the link. It does **not** scrape the
//! child's stdout for the run id: `pilot run` already honours `WINGMAN_RUN_ID`
//! (that is how a detached parent hands its id to the re-exec'd child), so the
//! board mints the id up front and knows it before the process starts.
//!
//! The decision half — id, argv, refusals — is [`plan_dispatch`], which is
//! pure and fully testable. The spawn itself is a handful of lines with no
//! branching worth a stub binary.

use std::path::{Path, PathBuf};

use crate::card::Card;
use crate::registry::Project;
use crate::store::{BoardError, BoardStore, Result};

/// Flags a caller may never forward to `pilot run` through the board.
///
/// `--worker-mode` is a pilot-internal contract (the same rule `serve`
/// applies); the detach and watch flags are ours to set, and letting a user
/// pass them would either double-detach or block the board on a tail.
const REFUSED: &[&str] = &["--worker-mode", "--detached", "-d", "--watch"];

#[derive(Debug, Clone, Default)]
pub struct DispatchOpts {
    /// Extra `pilot run` flags, forwarded verbatim.
    pub extra_args: Vec<String>,
    /// Dispatch even though a live dispatch already exists.
    pub again: bool,
}

/// What the spawn will do, decided before anything is launched.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchPlan {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub project_root: PathBuf,
    /// Argv after the executable.
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dispatched {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub pid: u32,
}

/// Generate a run id in pilot's format: `YYYY-MM-DD-HHMM-<rand6>`.
///
/// Deliberately mirrors `pilot::new_run_id` rather than importing it — that
/// function is private to the CLI, and a board-minted id must be
/// indistinguishable from a pilot-minted one on disk.
fn new_run_id() -> String {
    use rand::Rng;
    let suffix: String = rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(6)
        .map(|c| (c as char).to_ascii_lowercase())
        .collect();
    format!("{}-{suffix}", chrono::Utc::now().format("%Y-%m-%d-%H%M"))
}

/// Decide the run id, directory and argv for dispatching `card`.
pub fn plan_dispatch(card: &Card, project: &Project, opts: &DispatchOpts) -> Result<DispatchPlan> {
    if !project.exists() {
        return Err(BoardError::Invalid(format!(
            "project `{}` is missing at {} — use `wingman board projects --relocate`",
            project.id,
            project.root.display()
        )));
    }
    if let Some(bad) = opts
        .extra_args
        .iter()
        .find(|a| REFUSED.contains(&a.as_str()))
    {
        return Err(BoardError::Invalid(format!(
            "`{bad}` cannot be forwarded to pilot from the board"
        )));
    }

    let run_id = new_run_id();
    let mut args = vec![
        "pilot".to_string(),
        "run".to_string(),
        card.prompt().to_string(),
        "--detached".to_string(),
    ];
    args.extend(opts.extra_args.iter().cloned());

    Ok(DispatchPlan {
        run_dir: wingman_autonomous::run_dir(&project.root, &run_id),
        run_id,
        project_root: project.root.clone(),
        args,
    })
}

impl BoardStore {
    /// Dispatch a card, recording the link on success.
    pub fn dispatch_card(
        &self,
        card: &Card,
        project: &Project,
        opts: &DispatchOpts,
    ) -> Result<Dispatched> {
        if !opts.again {
            if let Some(d) = self.newest_dispatch(&card.id)? {
                if d.is_live()
                    && self
                        .rollup_for(&d.run_dir)?
                        .is_some_and(|r| !r.is_terminal())
                {
                    return Err(BoardError::Invalid(format!(
                        "card {} already has a live run ({}) — pass --again to start another",
                        card.short(),
                        d.run_id
                    )));
                }
            }
        }

        let plan = plan_dispatch(card, project, opts)?;
        let exe = std::env::current_exe().map_err(|source| BoardError::Io {
            path: PathBuf::from("current_exe"),
            source,
        })?;
        let pid = spawn(&exe, &plan)?;
        self.record_dispatch(&card.id, &project.id, &plan.run_id, &plan.run_dir)?;
        Ok(Dispatched {
            run_id: plan.run_id,
            run_dir: plan.run_dir,
            pid,
        })
    }
}

/// Launch the plan. `pilot run --detached` re-execs itself and returns
/// immediately, so this waits for the launcher, not for the run.
fn spawn(exe: &Path, plan: &DispatchPlan) -> Result<u32> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&plan.args)
        .current_dir(&plan.project_root)
        .env("WINGMAN_RUN_ID", &plan.run_id)
        .stdin(std::process::Stdio::null());

    let out = cmd.output().map_err(|source| BoardError::Io {
        path: exe.to_path_buf(),
        source,
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(BoardError::Invalid(format!(
            "pilot run failed ({}): {}",
            out.status,
            err.trim()
        )));
    }
    // The launcher's own pid is not the run's, but it is what we can observe
    // without parsing its output; the run id is the identifier that matters.
    Ok(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::NewCard;
    use crate::store::tests::store;

    fn fixture() -> (tempfile::TempDir, BoardStore, Project, Card) {
        let (dir, s) = store();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let pid = s.touch_project(&root).unwrap();
        let card = s
            .create_card(NewCard {
                project_id: pid.clone(),
                title: "Fix LSP".into(),
                goal: Some("stop the restart storm".into()),
                ..Default::default()
            })
            .unwrap();
        let project = s.project(&pid).unwrap();
        (dir, s, project, card)
    }

    #[test]
    fn plan_uses_the_goal_and_detaches() {
        let (_d, _s, p, c) = fixture();
        let plan = plan_dispatch(&c, &p, &DispatchOpts::default()).unwrap();
        assert_eq!(
            plan.args,
            vec!["pilot", "run", "stop the restart storm", "--detached"]
        );
        assert!(plan.run_dir.ends_with(&plan.run_id));
        assert!(plan.run_dir.to_string_lossy().contains("autonomous"));
    }

    #[test]
    fn plan_falls_back_to_the_title() {
        let (_d, s, p, _) = fixture();
        let c = s
            .create_card(NewCard {
                project_id: p.id.clone(),
                title: "Only a title".into(),
                ..Default::default()
            })
            .unwrap();
        let plan = plan_dispatch(&c, &p, &DispatchOpts::default()).unwrap();
        assert_eq!(plan.args[2], "Only a title");
    }

    #[test]
    fn extra_args_are_forwarded_verbatim() {
        let (_d, _s, p, c) = fixture();
        let opts = DispatchOpts {
            extra_args: vec![
                "--max-usd".into(),
                "5".into(),
                "--tier".into(),
                "auto".into(),
            ],
            again: false,
        };
        let plan = plan_dispatch(&c, &p, &opts).unwrap();
        assert_eq!(&plan.args[4..], &["--max-usd", "5", "--tier", "auto"]);
    }

    #[test]
    fn refused_flags_are_rejected() {
        let (_d, _s, p, c) = fixture();
        for bad in REFUSED {
            let opts = DispatchOpts {
                extra_args: vec![bad.to_string()],
                again: false,
            };
            assert!(
                plan_dispatch(&c, &p, &opts).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn missing_project_cannot_be_dispatched() {
        let (dir, s, _p, c) = fixture();
        let root = dir.path().join("gone");
        std::fs::create_dir_all(&root).unwrap();
        let id = s.touch_project(&root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let p = s.project(&id).unwrap();
        assert!(plan_dispatch(&c, &p, &DispatchOpts::default()).is_err());
    }

    #[test]
    fn run_ids_match_pilot_format() {
        let id = new_run_id();
        // YYYY-MM-DD-HHMM-xxxxxx
        assert_eq!(id.len(), 22, "{id}");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[4].len(), 6);
        assert!(parts[0].len() == 4 && parts[0].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn second_live_dispatch_is_refused_without_again() {
        let (dir, s, p, c) = fixture();
        // A live run: non-terminal state.json under the recorded run dir.
        let run = dir.path().join("run1");
        std::fs::create_dir_all(&run).unwrap();
        let mut st = wingman_autonomous::RunState::new("run1", "g", "base", "br");
        st.status = wingman_autonomous::RunStatus::Running;
        std::fs::write(run.join("state.json"), serde_json::to_string(&st).unwrap()).unwrap();
        s.record_dispatch(&c.id, &p.id, "run1", &run).unwrap();

        let err = s
            .dispatch_card(&c, &p, &DispatchOpts::default())
            .unwrap_err();
        assert!(format!("{err}").contains("already has a live run"), "{err}");
    }

    #[test]
    fn terminal_run_does_not_block_a_new_dispatch() {
        let (dir, s, p, c) = fixture();
        let run = dir.path().join("run1");
        std::fs::create_dir_all(&run).unwrap();
        let mut st = wingman_autonomous::RunState::new("run1", "g", "base", "br");
        st.status = wingman_autonomous::RunStatus::Done;
        std::fs::write(run.join("state.json"), serde_json::to_string(&st).unwrap()).unwrap();
        s.record_dispatch(&c.id, &p.id, "run1", &run).unwrap();

        // Gets past the live-dispatch guard and fails later, at the spawn.
        let err = s
            .dispatch_card(&c, &p, &DispatchOpts::default())
            .unwrap_err();
        assert!(
            !format!("{err}").contains("already has a live run"),
            "guard should not trip on a finished run: {err}"
        );
    }
}
