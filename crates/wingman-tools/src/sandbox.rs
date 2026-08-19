//! OS-level containment for `run_shell`.
//!
//! The permission modes confine which *paths the file tools* may touch, but
//! `run_shell` hands an arbitrary command to the OS: in `auto-edit` the agent
//! can write inside the project via `write_file` and simultaneously `cat
//! ~/.ssh/id_rsa` via the shell. A denylist can't close that — it is pattern
//! matching against an adversary who can spell things differently (`env`,
//! absolute paths, base64, heredocs). The only real boundary is the kernel's.
//!
//! So this wraps the command in whatever sandbox the platform actually
//! provides, rather than trying to implement one:
//!
//! | platform | mechanism                  | provides                        |
//! |----------|----------------------------|---------------------------------|
//! | Linux    | `bwrap` (bubblewrap)       | read-only `/`, writable project |
//! | macOS    | `sandbox-exec` (Seatbelt)  | read-only `/`, writable project |
//! | Windows  | Job Object                 | no orphans, no clipboard/handle |
//! |          |                            | theft, process + memory caps    |
//!
//! Deliberately integrating rather than building: both mechanisms are mature,
//! already present on most developer machines, and need no privileges.
//!
//! **Scope, honestly.** This confines the *filesystem*. It does not block
//! network egress — a sandboxed command can still `curl`. Filesystem
//! containment is what stops credential theft from `~/.ssh` and `~/.aws`,
//! which is the concrete failure this addresses; egress control is a separate
//! problem tracked separately.
//!
//! **Windows is the weak one, and says so.** A Job Object contains what a Job
//! Object contains: the process tree can't outlive its timeout, can't read the
//! clipboard, can't reach handles outside the job, and can't fork-bomb. It does
//! **not** scope the filesystem — `type %USERPROFILE%\.ssh\id_rsa` still
//! succeeds, where `bwrap`/Seatbelt refuse it. Path scoping on Windows needs
//! AppContainer or a restricted primary token, and both require Wingman to own
//! `CreateProcessW` (custom pipe plumbing) rather than spawning through
//! `tokio::process`. Tracked in
//! <https://github.com/vedantnimbarte/Wingman/issues/124>. Because the
//! guarantee is weaker, [`Availability::scopes_filesystem`] is what
//! `shell_sandbox = "required"` gates on — `required` on Windows still
//! refuses, so nobody's opt-in is silently downgraded.

use std::path::Path;

/// Which containment mechanism is available on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// `bwrap` found on PATH (Linux).
    Bubblewrap,
    /// `sandbox-exec` found (macOS).
    SeatBelt,
    /// Windows Job Object: lifetime, UI, and resource containment, but no
    /// filesystem scoping. See the module docs.
    JobObject,
    /// Nothing usable. On Linux install `bubblewrap`.
    None,
}

impl Availability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap (bwrap)",
            Self::SeatBelt => "macOS sandbox-exec",
            Self::JobObject => "Windows Job Object",
            Self::None => "none",
        }
    }

    pub fn is_some(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Does this mechanism confine which paths the command may touch?
    ///
    /// False for the Windows Job Object, which contains the process but not
    /// its filesystem access. `shell_sandbox = "required"` gates on this, so
    /// "required" keeps meaning "credential directories are out of reach".
    pub fn scopes_filesystem(self) -> bool {
        matches!(self, Self::Bubblewrap | Self::SeatBelt)
    }
}

/// Probe once per process — this shells out, and `run_shell` is hot.
pub fn availability() -> Availability {
    static CACHE: std::sync::OnceLock<Availability> = std::sync::OnceLock::new();
    *CACHE.get_or_init(detect)
}

fn detect() -> Availability {
    if cfg!(target_os = "linux") && which("bwrap") {
        return Availability::Bubblewrap;
    }
    if cfg!(target_os = "macos") && which("sandbox-exec") {
        return Availability::SeatBelt;
    }
    if cfg!(windows) {
        // Job Objects are a kernel primitive — always present, nothing to
        // probe for and nothing to install.
        return Availability::JobObject;
    }
    Availability::None
}

/// Is `program` on PATH? Uses the platform's own resolver so we inherit its
/// rules (PATHEXT on Windows, etc.).
fn which(program: &str) -> bool {
    let probe = if cfg!(windows) { "where" } else { "command" };
    let args: Vec<&str> = if cfg!(windows) {
        vec![program]
    } else {
        vec!["-v", program]
    };
    std::process::Command::new(probe)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the argv that runs `command` confined to `project_root`.
///
/// Returns `None` when no mechanism is available, so the caller can decide
/// whether to refuse (`required`) or proceed unconfined (`auto`).
/// The Windows Job Object is not an argv wrapper — it is applied to the child
/// after spawn by [`windows_job::confine`] — so this returns `None` there.
pub fn wrap(command: &str, project_root: &Path, tmp: &Path) -> Option<Vec<String>> {
    match availability() {
        Availability::Bubblewrap => Some(bwrap_argv(command, project_root, tmp)),
        Availability::SeatBelt => Some(seatbelt_argv(command, project_root, tmp)),
        Availability::JobObject | Availability::None => None,
    }
}

/// `bwrap` argv: bind the whole filesystem read-only, then re-bind the project
/// and a temp dir writable. `--die-with-parent` means an orphaned sandbox can't
/// outlive the agent.
fn bwrap_argv(command: &str, project_root: &Path, tmp: &Path) -> Vec<String> {
    let root = project_root.display().to_string();
    let tmp = tmp.display().to_string();
    vec![
        "bwrap".into(),
        // Everything visible, nothing writable...
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        // ...except the project and scratch space.
        "--bind".into(),
        root.clone(),
        root.clone(),
        "--bind".into(),
        tmp.clone(),
        tmp,
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--die-with-parent".into(),
        "--chdir".into(),
        root,
        "sh".into(),
        "-c".into(),
        command.into(),
    ]
}

/// Seatbelt profile: deny writes by default, allow them under the project and
/// the temp dir. Reads stay permitted — build tools need the toolchain, and
/// the threat being closed is credential *exfiltration by write* plus casual
/// reads of `~/.ssh` (see `deny file-read*` below for the sensitive set).
fn seatbelt_argv(command: &str, project_root: &Path, tmp: &Path) -> Vec<String> {
    let root = project_root.display().to_string();
    let tmp_s = tmp.display().to_string();
    let home = std::env::var("HOME").unwrap_or_default();

    let profile = format!(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write* (subpath \"{root}\"))\n\
         (allow file-write* (subpath \"{tmp_s}\"))\n\
         (allow file-write* (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))\n\
         (deny file-read* (subpath \"{home}/.ssh\") (subpath \"{home}/.aws\") (subpath \"{home}/.gnupg\"))\n"
    );

    vec![
        "sandbox-exec".into(),
        "-p".into(),
        profile,
        "sh".into(),
        "-c".into(),
        command.into(),
    ]
}

#[cfg(windows)]
pub mod windows_job;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bwrap_argv_binds_project_writable_and_root_readonly() {
        let argv = bwrap_argv("echo hi", Path::new("/home/u/proj"), Path::new("/tmp/x"));

        // Whole filesystem read-only...
        let ro = argv.windows(3).any(|w| w == ["--ro-bind", "/", "/"]);
        assert!(ro, "root should be bound read-only: {argv:?}");

        // ...project writable.
        let rw = argv
            .windows(3)
            .any(|w| w == ["--bind", "/home/u/proj", "/home/u/proj"]);
        assert!(rw, "project should be bound writable: {argv:?}");

        assert!(argv.contains(&"--die-with-parent".to_string()));
        assert_eq!(argv.last().unwrap(), "echo hi");
    }

    #[test]
    fn seatbelt_profile_denies_writes_then_allows_the_project() {
        let argv = seatbelt_argv("echo hi", Path::new("/Users/u/proj"), Path::new("/tmp/x"));
        let profile = &argv[2];

        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(allow file-write* (subpath \"/Users/u/proj\"))"));
        // The deny must come before the project allow, or it overrides it.
        let deny_at = profile.find("(deny file-write*)").unwrap();
        let allow_at = profile.find("(allow file-write* (subpath").unwrap();
        assert!(deny_at < allow_at, "deny-all must precede the allowances");
        assert_eq!(argv.last().unwrap(), "echo hi");
    }

    #[test]
    fn seatbelt_profile_blocks_credential_directories() {
        let argv = seatbelt_argv("x", Path::new("/p"), Path::new("/tmp"));
        let profile = &argv[2];
        assert!(profile.contains(".ssh"));
        assert!(profile.contains(".aws"));
    }

    #[test]
    fn availability_is_cached_and_consistent() {
        assert_eq!(availability(), availability());
    }

    #[test]
    fn only_the_unix_mechanisms_claim_filesystem_scoping() {
        assert!(Availability::Bubblewrap.scopes_filesystem());
        assert!(Availability::SeatBelt.scopes_filesystem());
        // The Windows Job Object contains the process, not its file access —
        // `required` must keep refusing there rather than silently accepting
        // a weaker guarantee.
        assert!(!Availability::JobObject.scopes_filesystem());
        assert!(Availability::JobObject.is_some());
        assert!(!Availability::None.scopes_filesystem());
    }

    #[test]
    fn the_job_object_is_not_an_argv_wrapper() {
        // `wrap` returning Some would make run_shell try to exec it.
        if availability() == Availability::JobObject {
            assert!(wrap("echo hi", Path::new("."), Path::new(".")).is_none());
        }
    }
}
