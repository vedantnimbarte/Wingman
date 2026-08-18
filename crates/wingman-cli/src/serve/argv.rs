//! What a request is allowed to ask the CLI to do.
//!
//! Both the table-driven read routes and `/v1/exec` build an argv and hand it
//! to a child process. This module is the one place that decides whether an
//! argv is acceptable, so there is a single list to audit rather than two
//! that drift.
//!
//! Three rules:
//!
//! 1. **Argv, never a shell string.** The child is spawned with an argument
//!    vector, so there is no shell to inject into — no `sh -c`, no quoting to
//!    get wrong, no `;` that means anything.
//! 2. **Known subcommands only**, and a refusal list on top. The known set is
//!    read from clap itself, so a subcommand added tomorrow is reachable
//!    without editing a list here, and a *typo* still 400s.
//! 3. **The ceiling binds.** A `--mode` above the server's ceiling is a
//!    refusal, not a downgrade, and the resolved mode is passed to the child
//!    explicitly.

use clap::CommandFactory;
use wingman_config::PermissionMode;

use super::rank;

/// Subcommands the API will not run, with the reason.
///
/// These are not "dangerous" in the abstract — `wingman login` is fine at a
/// terminal. They are wrong *over HTTP*: they either recurse, need an
/// interactive browser/console, or are an internal contract that only the
/// orchestrator should use.
const REFUSED: &[(&str, &str)] = &[
    ("serve", "would start a second server inside this one"),
    (
        "login",
        "needs an interactive browser/console flow; run it at the machine",
    ),
    (
        "logout",
        "credential removal should happen at the machine that holds them",
    ),
    (
        "tour",
        "interactive walkthrough; nothing useful comes back over HTTP",
    ),
];

/// Flags the API will not pass through, with the reason.
const REFUSED_FLAGS: &[(&str, &str)] = &[
    (
        "--worker-mode",
        "a pilot-internal contract; use the pilot routes",
    ),
    ("--task-file", "only meaningful with --worker-mode"),
];

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// 400 — malformed or unknown.
    BadRequest(String),
    /// 403 — asked for more authority than the ceiling allows.
    Forbidden(String),
}

/// Validate a request-supplied argv and return it with the effective mode
/// appended.
///
/// `--mode`/`--yolo` in the argv are honoured up to the ceiling and refused
/// above it. When the caller did not specify one, the ceiling is passed
/// explicitly rather than left to config: a server whose ceiling is
/// `read-only` must not run a turn at whatever `permission_mode` the config
/// file happens to say.
pub fn sanitize(args: &[String], ceiling: PermissionMode) -> Result<Vec<String>, Rejected> {
    let Some(first) = args.first() else {
        return Err(Rejected::BadRequest(
            "args must start with a subcommand".into(),
        ));
    };
    if first.starts_with('-') {
        return Err(Rejected::BadRequest(format!(
            "args must start with a subcommand, not the flag '{first}'"
        )));
    }
    if let Some((_, why)) = REFUSED.iter().find(|(name, _)| name == first) {
        return Err(Rejected::Forbidden(format!(
            "'{first}' is not available over the API: {why}"
        )));
    }
    if !known_subcommands().iter().any(|s| s == first) {
        return Err(Rejected::BadRequest(format!(
            "unknown subcommand '{first}'"
        )));
    }

    for (flag, why) in REFUSED_FLAGS {
        if args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
        {
            return Err(Rejected::Forbidden(format!("'{flag}' is refused: {why}")));
        }
    }

    // Resolve any requested mode against the ceiling.
    let mut requested: Option<PermissionMode> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--yolo" {
            requested = Some(PermissionMode::Yolo);
        } else if let Some(v) = a.strip_prefix("--mode=") {
            requested = Some(parse_mode(v)?);
        } else if a == "--mode" {
            let Some(v) = args.get(i + 1) else {
                return Err(Rejected::BadRequest("--mode needs a value".into()));
            };
            requested = Some(parse_mode(v)?);
            i += 1;
        }
        i += 1;
    }
    if let Some(m) = requested {
        if rank(m) > rank(ceiling) {
            return Err(Rejected::Forbidden(format!(
                "requested mode '{m}' exceeds this server's ceiling '{ceiling}'"
            )));
        }
    }

    // Strip whatever mode flags came in and append the resolved one, so the
    // child sees exactly one and it is the one policy chose.
    let mut out: Vec<String> = Vec::with_capacity(args.len() + 2);
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--mode" {
            skip_next = true;
            continue;
        }
        if a == "--yolo" || a.starts_with("--mode=") {
            continue;
        }
        out.push(a.clone());
    }
    out.push("--mode".into());
    out.push(requested.unwrap_or(ceiling).to_string());
    Ok(out)
}

fn parse_mode(v: &str) -> Result<PermissionMode, Rejected> {
    v.parse().map_err(|e: String| Rejected::BadRequest(e))
}

/// Every subcommand clap knows about, hidden ones included. Read from the
/// parser rather than duplicated here so it cannot fall behind the CLI.
fn known_subcommands() -> Vec<String> {
    crate::cli::Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_known_subcommand_passes_with_the_ceiling_appended() {
        let out = sanitize(&args(&["cost", "--json"]), PermissionMode::AutoEdit).unwrap();
        assert_eq!(out, args(&["cost", "--json", "--mode", "auto-edit"]));
    }

    #[test]
    fn unknown_subcommands_are_rejected() {
        let err = sanitize(
            &args(&["definitely-not-a-command"]),
            PermissionMode::AutoEdit,
        )
        .unwrap_err();
        assert!(matches!(err, Rejected::BadRequest(_)));
    }

    #[test]
    fn the_refusal_list_is_enforced_with_a_reason() {
        for name in ["serve", "login", "logout"] {
            match sanitize(&args(&[name]), PermissionMode::AutoEdit).unwrap_err() {
                Rejected::Forbidden(msg) => assert!(msg.contains(name), "{msg}"),
                other => panic!("{name} should be forbidden, got {other:?}"),
            }
        }
    }

    #[test]
    fn worker_mode_cannot_be_smuggled_in() {
        let err = sanitize(
            &args(&["pilot", "run", "--worker-mode"]),
            PermissionMode::AutoEdit,
        )
        .unwrap_err();
        assert!(matches!(err, Rejected::Forbidden(_)));
    }

    #[test]
    fn a_mode_above_the_ceiling_is_forbidden_not_downgraded() {
        let err = sanitize(
            &args(&["review", "--mode", "yolo"]),
            PermissionMode::AutoEdit,
        )
        .unwrap_err();
        match err {
            Rejected::Forbidden(msg) => assert!(msg.contains("ceiling"), "{msg}"),
            other => panic!("expected 403, got {other:?}"),
        }
        // The `--yolo` shorthand and `--mode=` spelling take the same path.
        assert!(matches!(
            sanitize(&args(&["review", "--yolo"]), PermissionMode::AutoEdit),
            Err(Rejected::Forbidden(_))
        ));
        assert!(matches!(
            sanitize(&args(&["review", "--mode=yolo"]), PermissionMode::AutoEdit),
            Err(Rejected::Forbidden(_))
        ));
    }

    #[test]
    fn a_mode_below_the_ceiling_is_honoured_and_appears_exactly_once() {
        let out = sanitize(
            &args(&["review", "--mode", "read-only"]),
            PermissionMode::AutoEdit,
        )
        .unwrap();
        assert_eq!(out.iter().filter(|a| *a == "--mode").count(), 1);
        assert_eq!(out.last().unwrap(), "read-only");
    }

    #[test]
    fn a_flag_cannot_masquerade_as_the_subcommand() {
        assert!(matches!(
            sanitize(&args(&["--mode", "yolo"]), PermissionMode::Yolo),
            Err(Rejected::BadRequest(_))
        ));
        assert!(matches!(
            sanitize(&[], PermissionMode::AutoEdit),
            Err(Rejected::BadRequest(_))
        ));
    }
}
