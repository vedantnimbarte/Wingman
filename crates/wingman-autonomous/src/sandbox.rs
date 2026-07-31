//! J11 — per-task isolation *classification*.
//!
//! **This module classifies risk. It does not isolate anything.** The name
//! "sandbox tier" has been a persistent source of confusion, so to be blunt:
//!
//!   - `IsolationTier::Host` — runs on your machine. (Everything does.)
//!   - `IsolationTier::Container` — *recommends* a container. Worker execution
//!     is not containerised; `run_in_container` wraps acceptance commands only,
//!     and `resolve_effective_tier` silently degrades to Host whenever no
//!     Docker daemon is reachable, which is most machines.
//!   - `IsolationTier::Vm` — the one tier with teeth, and only because it
//!     **fails closed**: `pilot run` refuses to start a vm-tier task rather
//!     than run it unisolated, unless the operator sets
//!     `[pilot.sandbox].allow_unsandboxed_vm_tasks`.
//!
//! Two further caveats worth stating, since they bound how much this is worth:
//! the tier is derived from the *model-produced plan* (`writes`,
//! `reversibility`), so a prompt injection that shapes the plan also shapes
//! its own tier; and the worker agent that performs the edits and calls
//! `run_shell` always runs on the host regardless of tier.
//!
//! Trust tier (E1) answers *whether* to approve. This answers *how risky the
//! work looks*, which drives the vm fail-closed gate and shows up in run
//! reports. For real containment of untrusted code, see the OS-level sandbox
//! work tracked in
//! [#111](https://github.com/vedantnimbarte/Wingman/issues/111) — do not rely
//! on this module for it.

use crate::model::{Acceptance, Reversibility, Task};

/// Former name. Renamed because it described containment this module does
/// not provide; see the module docs.
#[deprecated(note = "renamed to IsolationTier: this classifies risk, it does not isolate")]
pub type SandboxTier = IsolationTier;
use crate::pr::{CommandOut, CommandRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IsolationTier {
    Host,
    Container,
    Vm,
}

impl IsolationTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
            Self::Vm => "vm",
        }
    }

    pub fn parse(s: &str) -> IsolationTier {
        match s.trim().to_ascii_lowercase().as_str() {
            "container" => Self::Container,
            "vm" => Self::Vm,
            _ => Self::Host,
        }
    }
}

/// Path fragments that imply dependency/build changes → at least container.
const DEP_MARKERS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "build.rs",
    "requirements.txt",
    "pyproject.toml",
    "go.mod",
];

/// Path fragments that imply infra/migration changes → vm.
const VM_MARKERS: &[&str] = &[
    "migration",
    "migrations",
    "terraform",
    "Dockerfile",
    "/infra/",
    "helm",
];

/// Acceptance-command substrings that imply running untrusted/network/build
/// work → at least container.
const RISKY_CMDS: &[&str] = &[
    "docker",
    "deploy",
    "curl",
    "wget",
    "npm install",
    "cargo install",
];

fn writes_match(task: &Task, markers: &[&str]) -> bool {
    task.writes
        .iter()
        .any(|w| markers.iter().any(|m| w.contains(m)))
}

fn acceptance_matches(task: &Task, needles: &[&str]) -> bool {
    task.acceptance.iter().any(|a| match a {
        Acceptance::Shell { cmd } => needles.iter().any(|n| cmd.contains(n)),
        Acceptance::Run { script, target } => {
            let hay = script.as_deref().unwrap_or(target);
            needles.iter().any(|n| hay.contains(n))
        }
        _ => false,
    })
}

/// Select the sandbox tier for a task. The result is the *max* of: the
/// configured `default_tier`, the reversibility floor, and any escalation
/// implied by the task's writes/acceptance.
pub fn select_tier(task: &Task, default_tier: IsolationTier) -> IsolationTier {
    let mut tier = default_tier;

    // Reversibility floor.
    tier = tier.max(match task.reversibility {
        Reversibility::Irreversible => IsolationTier::Vm,
        Reversibility::Hard => IsolationTier::Container,
        Reversibility::Trivial => IsolationTier::Host,
    });

    // Infra/migration writes → vm.
    if writes_match(task, VM_MARKERS) {
        tier = tier.max(IsolationTier::Vm);
    }
    // Dependency/build writes or risky acceptance commands → container.
    if writes_match(task, DEP_MARKERS) || acceptance_matches(task, RISKY_CMDS) {
        tier = tier.max(IsolationTier::Container);
    }

    tier
}

/// J11 executor shell: build the `docker run` argv to execute `cmd`
/// inside `image` with the worktree mounted read-write at `/work`. The
/// orchestrator runs this via a `CommandRunner`; keeping the argv pure
/// makes the (otherwise Docker-dependent) command testable.
pub fn container_run_argv(image: &str, worktree_host_path: &str, cmd: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        "--rm".to_string(),
        "-v".to_string(),
        format!("{worktree_host_path}:/work"),
        "-w".to_string(),
        "/work".to_string(),
        image.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        cmd.to_string(),
    ]
}

/// J11 availability probe: is a Docker daemon reachable? Runs
/// `docker version` (which contacts the daemon, unlike `--version`) and
/// reports success. Used to decide whether a container/vm tier can
/// actually be honored or must degrade to host.
pub fn docker_available(runner: &dyn CommandRunner) -> bool {
    runner
        .run("docker", &["version"], std::path::Path::new("."))
        .map(|o| o.success())
        .unwrap_or(false)
}

/// Resolve the tier that will *actually* be used: a `container`/`vm`
/// request degrades to `host` when no Docker daemon is reachable, so a run
/// never wedges on a missing executor. Returns `(effective, degraded)`
/// where `degraded` is true when the request was downgraded (caller
/// warns).
pub fn resolve_effective_tier(
    requested: IsolationTier,
    runner: &dyn CommandRunner,
) -> (IsolationTier, bool) {
    match requested {
        IsolationTier::Host => (IsolationTier::Host, false),
        IsolationTier::Container | IsolationTier::Vm => {
            if docker_available(runner) {
                (requested, false)
            } else {
                (IsolationTier::Host, true)
            }
        }
    }
}

/// J11 executor invocation: run `cmd` inside `image` via `docker run`
/// (built by [`container_run_argv`]) through a [`CommandRunner`]. The
/// runner seam keeps it testable; the production caller passes
/// [`crate::pr::SystemCommandRunner`], which needs a Docker daemon.
pub fn run_in_container(
    runner: &dyn CommandRunner,
    image: &str,
    worktree_host_path: &str,
    cmd: &str,
) -> std::io::Result<CommandOut> {
    let argv = container_run_argv(image, worktree_host_path, cmd);
    let args: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    runner.run("docker", &args, std::path::Path::new(worktree_host_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Acceptance, Role, Task};
    use crate::pr::CommandOut;
    use std::sync::Mutex;

    fn task(writes: &[&str], rev: Reversibility) -> Task {
        let mut t = Task::new("t1", Role::Developer, "x");
        t.writes = writes.iter().map(|s| s.to_string()).collect();
        t.reversibility = rev;
        t
    }

    #[test]
    fn tier_parse_and_str() {
        assert_eq!(IsolationTier::parse("VM"), IsolationTier::Vm);
        assert_eq!(IsolationTier::parse("container"), IsolationTier::Container);
        assert_eq!(IsolationTier::parse("anything"), IsolationTier::Host);
        assert_eq!(IsolationTier::Vm.as_str(), "vm");
    }

    #[test]
    fn plain_edit_stays_host() {
        let t = task(&["crates/cli/src/main.rs"], Reversibility::Trivial);
        assert_eq!(select_tier(&t, IsolationTier::Host), IsolationTier::Host);
    }

    #[test]
    fn dependency_change_goes_container() {
        let t = task(&["Cargo.toml"], Reversibility::Trivial);
        assert_eq!(
            select_tier(&t, IsolationTier::Host),
            IsolationTier::Container
        );
    }

    #[test]
    fn migration_goes_vm() {
        let t = task(&["db/migrations/001_init.sql"], Reversibility::Trivial);
        assert_eq!(select_tier(&t, IsolationTier::Host), IsolationTier::Vm);
    }

    #[test]
    fn irreversible_floor_is_vm() {
        let t = task(&["crates/cli/src/main.rs"], Reversibility::Irreversible);
        assert_eq!(select_tier(&t, IsolationTier::Host), IsolationTier::Vm);
    }

    #[test]
    fn hard_floor_is_container() {
        let t = task(&["crates/cli/src/main.rs"], Reversibility::Hard);
        assert_eq!(
            select_tier(&t, IsolationTier::Host),
            IsolationTier::Container
        );
    }

    #[test]
    fn risky_acceptance_command_goes_container() {
        let mut t = task(&["crates/cli/src/main.rs"], Reversibility::Trivial);
        t.acceptance = vec![Acceptance::Shell {
            cmd: "docker build .".into(),
        }];
        assert_eq!(
            select_tier(&t, IsolationTier::Host),
            IsolationTier::Container
        );
    }

    #[test]
    fn default_tier_is_a_floor() {
        let t = task(&["crates/cli/src/main.rs"], Reversibility::Trivial);
        // Even a trivial edit respects a container default.
        assert_eq!(
            select_tier(&t, IsolationTier::Container),
            IsolationTier::Container
        );
    }

    #[test]
    fn container_argv_mounts_and_runs() {
        let argv = container_run_argv("wingman/sandbox:latest", "/home/u/wt", "cargo test");
        assert_eq!(argv[0], "run");
        assert!(argv.iter().any(|a| a == "/home/u/wt:/work"));
        assert!(argv.iter().any(|a| a == "wingman/sandbox:latest"));
        assert_eq!(argv.last().unwrap(), "cargo test");
    }

    struct DockerProbe {
        present: bool,
    }
    impl CommandRunner for DockerProbe {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            _c: &std::path::Path,
        ) -> std::io::Result<CommandOut> {
            let ok =
                !(program == "docker" && args.first().copied() == Some("version") && !self.present);
            Ok(CommandOut {
                status: Some(if ok { 0 } else { 1 }),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn docker_available_reflects_daemon() {
        assert!(docker_available(&DockerProbe { present: true }));
        assert!(!docker_available(&DockerProbe { present: false }));
    }

    #[test]
    fn host_tier_never_degrades() {
        let (eff, degraded) =
            resolve_effective_tier(IsolationTier::Host, &DockerProbe { present: false });
        assert_eq!(eff, IsolationTier::Host);
        assert!(!degraded);
    }

    #[test]
    fn container_tier_kept_when_docker_present() {
        let (eff, degraded) =
            resolve_effective_tier(IsolationTier::Container, &DockerProbe { present: true });
        assert_eq!(eff, IsolationTier::Container);
        assert!(!degraded);
    }

    #[test]
    fn container_tier_degrades_to_host_without_docker() {
        let (eff, degraded) =
            resolve_effective_tier(IsolationTier::Container, &DockerProbe { present: false });
        assert_eq!(eff, IsolationTier::Host);
        assert!(degraded);
    }

    #[test]
    fn vm_tier_degrades_to_host_without_docker() {
        let (eff, degraded) =
            resolve_effective_tier(IsolationTier::Vm, &DockerProbe { present: false });
        assert_eq!(eff, IsolationTier::Host);
        assert!(degraded);
    }

    #[test]
    fn run_in_container_invokes_docker_with_argv() {
        struct Rec {
            calls: Mutex<Vec<Vec<String>>>,
        }
        impl CommandRunner for Rec {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                _cwd: &std::path::Path,
            ) -> std::io::Result<CommandOut> {
                if program == "docker" {
                    self.calls
                        .lock()
                        .unwrap()
                        .push(args.iter().map(|s| s.to_string()).collect());
                }
                Ok(CommandOut {
                    status: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        let rec = Rec {
            calls: Mutex::new(Vec::new()),
        };
        let out = run_in_container(&rec, "img:latest", "/wt", "cargo test").unwrap();
        assert!(out.success());
        let calls = rec.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "run");
        assert!(calls[0].iter().any(|a| a == "/wt:/work"));
        assert_eq!(calls[0].last().unwrap(), "cargo test");
    }

    #[test]
    fn highest_signal_wins() {
        // Both a dep change (container) and a migration (vm) → vm.
        let t = task(&["Cargo.toml", "migrations/x.sql"], Reversibility::Trivial);
        assert_eq!(select_tier(&t, IsolationTier::Host), IsolationTier::Vm);
    }
}
