//! Black-box smoke tests for the compiled `wingman` binary.
//!
//! These are the first cross-boundary integration tests in the workspace: they
//! run the real binary (via Cargo's `CARGO_BIN_EXE_wingman`) end-to-end, so the
//! CLI wiring — arg parsing, subcommand dispatch, exit codes — is exercised as
//! a user would hit it, not just unit-tested per crate.
//!
//! They deliberately avoid network / provider calls (run in CI without API
//! keys) and never write to the developer's real `~/.wingman`: the only
//! mutating command exercised (`wingman init`) writes `WINGMAN.md` into an
//! isolated scratch *project* directory (its cwd), not the global home.

use std::path::PathBuf;
use std::process::Command;

/// A `Command` for the freshly built `wingman` binary for this test run.
fn wingman() -> Command {
    Command::new(env!("CARGO_BIN_EXE_wingman"))
}

#[test]
fn help_exits_zero_and_lists_subcommands() {
    let out = wingman().arg("--help").output().expect("run --help");
    assert!(out.status.success(), "--help should exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    // A few stable, load-bearing subcommands should always be advertised.
    for sub in ["config", "review", "knows", "pilot", "session"] {
        assert!(
            text.contains(sub),
            "--help output missing subcommand `{sub}`"
        );
    }
}

#[test]
fn help_lists_newly_merged_subcommands() {
    // Guards that the differentiation features merged into the CLI actually
    // wired their subcommands into clap (regression guard for the merge).
    let out = wingman().arg("--help").output().expect("run --help");
    let text = String::from_utf8_lossy(&out.stdout);
    for sub in ["distill", "indexd", "router", "rewind"] {
        assert!(
            text.contains(sub),
            "--help output missing merged subcommand `{sub}`"
        );
    }
}

#[test]
fn version_flag_prints_a_version() {
    let out = wingman().arg("--version").output().expect("run --version");
    assert!(out.status.success(), "--version should exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("wingman"), "--version should name the binary");
}

#[test]
fn unknown_subcommand_fails_cleanly() {
    let out = wingman()
        .arg("definitely-not-a-command")
        .output()
        .expect("run bogus subcommand");
    assert!(
        !out.status.success(),
        "an unknown subcommand must exit non-zero, not panic or succeed"
    );
    // clap writes usage to stderr; assert it's a clean parse error, not a panic.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("panicked"),
        "unknown subcommand should not panic: {err}"
    );
}

#[test]
fn bad_model_flag_errors_without_panicking() {
    // A `--model` value we can't resolve must produce a clean error, never a
    // panic (regression guard for `split_once('/')`-style parsing).
    let s = Scratch::new();
    let out = wingman()
        .args(["--model", "no-such-provider/no-such-model", "--print", "hi"])
        .current_dir(&s.dir)
        .output()
        .expect("run with bad model");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("panicked"),
        "an unresolvable --model must not panic: {combined}"
    );
    assert!(
        !out.status.success(),
        "an unresolvable --model should exit non-zero"
    );
}

#[test]
fn init_writes_wingman_md_into_isolated_project() {
    // `wingman init` writes WINGMAN.md into the project root, which (for a dir
    // with no .git/.wingman marker) is the cwd itself — so a scratch cwd fully
    // isolates the side effect from the real home.
    let s = Scratch::new();
    // Give the project something to introspect.
    std::fs::write(s.dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let out = wingman()
        .arg("init")
        .current_dir(&s.dir)
        .output()
        .expect("run init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        s.dir.join("WINGMAN.md").exists(),
        "init did not write WINGMAN.md into the project dir"
    );
}

#[test]
fn pilot_help_lists_subcommands() {
    // Guards that pilot-mode CLI wiring stays intact (pilot is user-validated
    // against live providers, not CI-validated end-to-end — so at minimum keep
    // the command surface from silently rotting).
    let out = wingman()
        .args(["pilot", "--help"])
        .output()
        .expect("run pilot --help");
    assert!(out.status.success(), "pilot --help should exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    for sub in ["run", "status", "watch", "resume"] {
        assert!(
            text.contains(sub),
            "pilot --help missing subcommand `{sub}`"
        );
    }
}

#[test]
fn pilot_status_without_runs_does_not_panic() {
    // `pilot status` reads run artifacts under .wingman/autonomous and needs no
    // provider. In an empty scratch project it must exit cleanly (no runs), not
    // panic.
    let s = Scratch::new();
    let out = wingman()
        .args(["pilot", "status"])
        .current_dir(&s.dir)
        .output()
        .expect("run pilot status");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("panicked"),
        "pilot status must not panic on an empty project: {combined}"
    );
}

/// Minimal, dependency-free scratch-dir helper. Kept in-file so these smoke
/// tests pull in nothing beyond std. Cleaned up on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let unique = format!(
            "wingman-smoke-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
