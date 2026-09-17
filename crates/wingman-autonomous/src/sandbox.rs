//! J11 — per-task isolation tiers.
//!
//! Every task is *classified* into a tier by [`select_tier`], from the
//! model-produced plan (`writes`, `reversibility`, acceptance commands):
//!
//!   - `IsolationTier::Host` — the worker runs on your machine, in its git
//!     worktree.
//!   - `IsolationTier::Container` — the whole worker subprocess runs in
//!     `docker run` against a *copy* of the worktree, under the CPU, memory,
//!     pid and network limits in `[pilot.sandbox]`. When it reports
//!     `task_complete`, the diff it produced is applied back to the host
//!     worktree and committed. With no Docker daemon the task degrades to
//!     host, as it always has.
//!   - `IsolationTier::Vm` — the same, inside a Firecracker microVM (Linux +
//!     KVM, optionally under the jailer): the copy is packed into an ext4
//!     drive, and the guest writes its diff to a raw output drive. When no
//!     vm backend is available the tier **fails closed**: `pilot run` refuses
//!     the task unless `[pilot.sandbox].allow_unsandboxed_vm_tasks` is set, in
//!     which case it gets the container tier if Docker is up, else host.
//!
//! What the tiers do not cover, so nobody leans on them for it:
//!
//!   - The tier is derived from the plan, so a prompt injection that shapes
//!     the plan also shapes its own tier.
//!   - The worker needs its model provider, so a sandbox has whatever network
//!     the config gives it, and the provider key is forwarded into it.
//!   - The patch that comes back is untrusted input. `git apply` refuses
//!     paths under `.git` and through symlinks, and the result lands on the
//!     task branch where review and the PR see it — nothing more.
//!   - **Neither backend has been run against a real Docker daemon or
//!     Firecracker host.** The argv, config, staging and patch-back are
//!     tested with mock runners; see `docs/PILOT-MODE.md`.
//!
//! Trust tier (E1) answers *whether* to approve. This answers *where the
//! work runs*.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use wingman_config::PilotSandboxConfig;

use crate::model::{Acceptance, Reversibility, Task};

/// Former name, kept so existing imports compile.
#[deprecated(note = "renamed to IsolationTier")]
pub type SandboxTier = IsolationTier;
use crate::pr::CommandRunner;

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

/// J11 availability probe: is a Docker daemon reachable? Runs
/// `docker version` (which contacts the daemon, unlike `--version`) and
/// reports success.
pub fn docker_available(runner: &dyn CommandRunner) -> bool {
    runner
        .run("docker", &["version"], Path::new("."))
        .map(|o| o.success())
        .unwrap_or(false)
}

/// Which non-host tiers this machine can honour. Probed once per run (and by
/// `wingman doctor`), then consulted per task.
#[derive(Debug, Clone)]
pub struct TierAvailability {
    pub docker: bool,
    /// `Err(reason)` when the vm tier cannot run here, which keeps it
    /// fail-closed.
    pub vm: Result<(), String>,
}

impl TierAvailability {
    pub fn probe(cfg: &PilotSandboxConfig, runner: &dyn CommandRunner) -> Self {
        Self {
            docker: docker_available(runner),
            vm: vm_available(cfg, runner, std::env::consts::OS, Path::new("/dev/kvm")),
        }
    }
}

/// Can the Firecracker vm tier run? `os` and `kvm` are parameters so the
/// Linux branch is testable everywhere. Every missing piece is a reason, and
/// any reason keeps the tier closed.
pub fn vm_available(
    cfg: &PilotSandboxConfig,
    runner: &dyn CommandRunner,
    os: &str,
    kvm: &Path,
) -> Result<(), String> {
    if os != "linux" {
        return Err(format!("the vm tier needs Linux with KVM; this is {os}"));
    }
    if cfg.vm_provider != "firecracker" {
        return Err(format!(
            "vm_provider \"{}\" is not implemented; only \"firecracker\" is",
            cfg.vm_provider
        ));
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(kvm)
        .map_err(|e| format!("{} is not usable: {e}", kvm.display()))?;
    let vm = &cfg.vm;
    for (key, path) in [
        ("kernel_image", &vm.kernel_image),
        ("rootfs_image", &vm.rootfs_image),
    ] {
        if path.is_empty() {
            return Err(format!("[pilot.sandbox.vm].{key} is not set"));
        }
        if !Path::new(path).is_file() {
            return Err(format!("[pilot.sandbox.vm].{key} {path} does not exist"));
        }
    }
    let runs = |bin: &str, arg: &str| {
        runner
            .run(bin, &[arg], Path::new("."))
            .map(|o| o.success())
            .unwrap_or(false)
    };
    for (bin, arg) in [(vm.firecracker_bin.as_str(), "--version"), ("mke2fs", "-V")] {
        if !runs(bin, arg) {
            return Err(format!("`{bin}` is not runnable"));
        }
    }
    if vm.use_jailer {
        // The jailer copies the exec file into the chroot by path.
        if !Path::new(&vm.firecracker_bin).is_absolute() {
            return Err("use_jailer needs an absolute [pilot.sandbox.vm].firecracker_bin".into());
        }
        if !runs(&vm.jailer_bin, "--version") {
            return Err(format!("`{}` is not runnable", vm.jailer_bin));
        }
        let root = runner
            .run("id", &["-u"], Path::new("."))
            .map(|o| o.success() && o.stdout.trim() == "0")
            .unwrap_or(false);
        if !root {
            return Err("the jailer needs root (or set use_jailer = false)".into());
        }
    }
    Ok(())
}

/// Resolve the tier that will *actually* be used. Returns `(effective,
/// degraded)`; `degraded` means weaker isolation than requested, which the
/// caller warns about.
///
/// A vm request only reaches the degraded arm after `pilot run` has refused
/// it or the operator set `allow_unsandboxed_vm_tasks`; it then takes the
/// best tier left.
pub fn resolve_effective_tier(
    requested: IsolationTier,
    avail: &TierAvailability,
) -> (IsolationTier, bool) {
    let container = if avail.docker {
        IsolationTier::Container
    } else {
        IsolationTier::Host
    };
    match requested {
        IsolationTier::Host => (IsolationTier::Host, false),
        IsolationTier::Container => (container, !avail.docker),
        IsolationTier::Vm if avail.vm.is_ok() => (IsolationTier::Vm, false),
        IsolationTier::Vm => (container, true),
    }
}

/// Where a sandboxed worker runs, handed to [`crate::worker::run_worker`].
#[derive(Debug, Clone)]
pub struct WorkerSandbox {
    /// `Container` or `Vm`.
    pub tier: IsolationTier,
    pub config: PilotSandboxConfig,
    /// Global `config.toml`, copied in so the worker resolves its provider.
    pub global_config: Option<PathBuf>,
}

/// The worktree copy is mounted (container) or packed (vm) here.
pub const GUEST_WORK: &str = "/work";
/// Wingman's files inside the copy; excluded from the patch.
const SANDBOX_DIR: &str = ".wingman-sandbox";
/// Guest init the vm rootfs must provide (see `docs/PILOT-MODE.md`).
const GUEST_INIT: &str = "/sbin/wingman-sandbox-init";
/// Raw output drive the vm guest writes its patch to.
// ponytail: fixed 64 MiB ceiling on a vm task's diff; size it from config if
// real tasks outgrow it.
const PATCH_DRIVE_BYTES: u64 = 64 * 1024 * 1024;
/// Last line of a completely written patch. Without it the diff failed or was
/// cut short (a VM's exit code does not reach the host), and applying what is
/// there would silently drop the rest of the worker's changes.
const PATCH_END: &str = "# wingman-sandbox: patch complete\n";

/// One prepared sandbox: the command to spawn, and the scratch files behind
/// it. Dropping it removes the scratch (and any leftover container).
#[derive(Debug)]
pub struct SandboxRun {
    pub program: String,
    pub args: Vec<String>,
    tier: IsolationTier,
    scratch: PathBuf,
    copy: PathBuf,
    /// vm only: the raw drive the guest writes its patch to.
    patch_drive: Option<PathBuf>,
    /// vm + jailer only: `<chroot_base>/<exec>/<id>`, removed on drop.
    jail: Option<PathBuf>,
    /// container only: removed with `docker rm -f` on drop, since killing the
    /// `docker run` client on timeout does not stop the container.
    container: Option<String>,
}

impl Drop for SandboxRun {
    fn drop(&mut self) {
        if let Some(name) = &self.container {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .output();
        }
        if let Some(jail) = &self.jail {
            let _ = std::fs::remove_dir_all(jail);
        }
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// Letters, digits and `-`, at most 64 — valid as a container name and a
/// jailer id.
fn sandbox_id(session_id: &str) -> String {
    let id: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(56)
        .collect();
    format!("wingman-{id}")
}

/// `[pilot.sandbox].env` entries are variable names; anything else is skipped.
fn is_env_name(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The script the sandbox runs: snapshot the copy as a base commit, run the
/// worker, then write everything it changed (committed or not) as a binary
/// diff to `patch_out`. The worker's exit code is preserved.
pub fn guest_script(
    worker_args: &[String],
    exports: &[(String, String)],
    patch_out: &str,
) -> String {
    let mut s =
        String::from("#!/bin/sh\n# Written by wingman for one sandboxed pilot worker (J11).\n");
    s.push_str(&format!("cd {GUEST_WORK} || exit 97\n"));
    s.push_str(&format!(
        "export HOME=/tmp WINGMAN_HOME={GUEST_WORK}/{SANDBOX_DIR}/home\n"
    ));
    s.push_str(
        "export GIT_AUTHOR_NAME='wingman pilot' GIT_AUTHOR_EMAIL=pilot@wingman.local \
         GIT_COMMITTER_NAME='wingman pilot' GIT_COMMITTER_EMAIL=pilot@wingman.local\n",
    );
    // A packed drive keeps host uids, which git run as the guest's root
    // would otherwise refuse as dubious ownership.
    s.push_str(
        "export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'\n",
    );
    for (k, v) in exports {
        s.push_str(&format!("export {k}={}\n", sh_quote(v)));
    }
    s.push_str(&format!(
        "git init -q . && printf '/{SANDBOX_DIR}/\\n/.wingman/\\n' >> .git/info/exclude \
         && git add -A && git commit -q --allow-empty -m 'sandbox base' || exit 97\n"
    ));
    s.push_str("base=$(git rev-parse HEAD)\nwingman");
    for a in worker_args {
        s.push(' ');
        s.push_str(&sh_quote(a));
    }
    s.push_str(&format!(
        "\nrc=$?\ngit add -A && git diff --cached --binary \"$base\" > /tmp/wingman.patch \
         && printf '{}' >> /tmp/wingman.patch && cat /tmp/wingman.patch > {} && sync || exit 98\n\
         exit $rc\n",
        PATCH_END.replace('\n', "\\n"),
        sh_quote(patch_out)
    ));
    s
}

/// `docker run` argv for a container-tier worker. Env vars are passed by
/// name only, so their values never appear in argv. `user` is the host
/// owner of the copy (Unix), so files the worker writes stay removable.
pub fn container_worker_argv(
    cfg: &PilotSandboxConfig,
    copy: &Path,
    name: &str,
    user: Option<(u32, u32)>,
) -> Vec<String> {
    container_argv(
        cfg,
        copy,
        name,
        user,
        &cfg.container_image,
        &["sh".into(), format!("{GUEST_WORK}/{SANDBOX_DIR}/run.sh")],
    )
}

/// `docker run` argv under `[pilot.sandbox]`'s limits: `mount` at
/// [`GUEST_WORK`], then `image` running `command`. The shared half of
/// [`container_worker_argv`], also used by `wingman bg --devcontainer`.
pub fn container_argv(
    cfg: &PilotSandboxConfig,
    mount: &Path,
    name: &str,
    user: Option<(u32, u32)>,
    image: &str,
    command: &[String],
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "-i".into(),
        "--name".into(),
        name.into(),
        "--network".into(),
        cfg.network.clone(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cpus".into(),
        cfg.cpus.to_string(),
        "--memory".into(),
        format!("{}m", cfg.memory_mib),
        "--pids-limit".into(),
        cfg.pids_limit.to_string(),
    ];
    if let Some((uid, gid)) = user {
        a.extend(["--user".into(), format!("{uid}:{gid}")]);
    }
    // A `NAME=value` entry would put the value in argv, so only names pass.
    for e in cfg.env.iter().filter(|k| is_env_name(k)) {
        a.extend(["-e".into(), e.clone()]);
    }
    a.extend([
        "-v".into(),
        format!("{}:{GUEST_WORK}", mount.display()),
        "-w".into(),
        GUEST_WORK.into(),
        image.into(),
    ]);
    a.extend(command.iter().cloned());
    a
}

/// Firecracker `--config-file` body: read-only rootfs, the worktree drive,
/// the raw patch drive, and a tap interface when one is configured.
pub fn firecracker_config(
    cfg: &PilotSandboxConfig,
    kernel: &str,
    rootfs: &str,
    work: &str,
    patch: &str,
) -> serde_json::Value {
    let drive = |id: &str, path: &str, root: bool, ro: bool| {
        serde_json::json!({
            "drive_id": id,
            "path_on_host": path,
            "is_root_device": root,
            "is_read_only": ro,
        })
    };
    let mut v = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel,
            "boot_args": format!("console=ttyS0 reboot=k panic=1 pci=off quiet init={GUEST_INIT}"),
        },
        "drives": [
            drive("rootfs", rootfs, true, true),
            drive("work", work, false, false),
            drive("patch", patch, false, false),
        ],
        "machine-config": {
            "vcpu_count": cfg.cpus,
            "mem_size_mib": cfg.memory_mib,
        },
    });
    if !cfg.vm.tap_device.is_empty() {
        v["network-interfaces"] = serde_json::json!([{
            "iface_id": "eth0",
            "host_dev_name": cfg.vm.tap_device,
        }]);
    }
    v
}

/// Argv for Firecracker itself: no API socket, everything from the config
/// file.
pub fn firecracker_argv(config_file: &str) -> Vec<String> {
    vec![
        "--no-api".into(),
        "--config-file".into(),
        config_file.into(),
    ]
}

/// Argv for the jailer, which chroots into
/// `<chroot_base_dir>/<exec>/<id>/root`, drops to `jailer_uid`/`jailer_gid`
/// and execs Firecracker there, so the config file and drives are named
/// relative to that root.
pub fn jailer_argv(cfg: &PilotSandboxConfig, id: &str) -> Vec<String> {
    let vm = &cfg.vm;
    let mut a: Vec<String> = vec![
        "--id".into(),
        id.into(),
        "--exec-file".into(),
        vm.firecracker_bin.clone(),
        "--uid".into(),
        vm.jailer_uid.to_string(),
        "--gid".into(),
        vm.jailer_gid.to_string(),
        "--chroot-base-dir".into(),
        vm.chroot_base_dir.clone(),
        "--".into(),
    ];
    a.extend(firecracker_argv("config.json"));
    a
}

/// Copy a worktree for a sandbox, skipping `.git` (the sandbox makes its own
/// base commit). Symlinks are recreated on Unix; elsewhere a symlink cannot be
/// copied faithfully, and a copy that turned it into a file would come back
/// in the patch as a type change, so it is refused.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for e in entries {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        let kind = e.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&from).map_err(|e| e.to_string())?;
                std::os::unix::fs::symlink(target, &to).map_err(|e| e.to_string())?;
            }
            #[cfg(not(unix))]
            return Err(format!(
                "{} is a symlink, which a sandbox copy cannot preserve on this OS",
                from.display()
            ));
        } else if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// Hard-link a big read-only image into the jail, copying across filesystems.
// ponytail: a cross-filesystem rootfs is copied per task; keep the chroot base
// on the images' filesystem, or share one staged copy if that gets slow.
fn stage(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::hard_link(from, to)
        .or_else(|_| std::fs::copy(from, to).map(|_| ()))
        .map_err(|e| format!("staging {} into the jail: {e}", from.display()))
}

/// The host uid/gid owning `path` (Unix), for `docker run --user`.
#[cfg(unix)]
pub fn owner_of(path: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| (m.uid(), m.gid()))
}
#[cfg(not(unix))]
pub fn owner_of(_path: &Path) -> Option<(u32, u32)> {
    None
}

/// Build the sandbox for one worker: copy `worktree` under `scratch_root`,
/// write the guest script and config into the copy, and — for the vm tier —
/// pack it into an ext4 drive and stage the jail. `worker_args` are the
/// `wingman` arguments as the guest sees them (paths under [`GUEST_WORK`]).
/// Nothing is spawned except `mke2fs`; the caller spawns
/// [`SandboxRun::program`].
pub fn prepare(
    sb: &WorkerSandbox,
    worktree: &Path,
    session_id: &str,
    worker_args: &[String],
    runner: &dyn CommandRunner,
    scratch_root: &Path,
) -> Result<SandboxRun, String> {
    let id = sandbox_id(session_id);
    let scratch = scratch_root.join(&id);
    let copy = scratch.join("work");
    // A stale scratch from a crashed run would leak into the copy.
    let _ = std::fs::remove_dir_all(&scratch);
    let mut run = SandboxRun {
        program: String::new(),
        args: Vec::new(),
        tier: sb.tier,
        scratch: scratch.clone(),
        copy: copy.clone(),
        patch_drive: None,
        jail: None,
        container: None,
    };
    copy_tree(worktree, &copy)?;
    let home = copy.join(SANDBOX_DIR).join("home");
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    if let Some(cfg) = sb.global_config.as_deref().filter(|p| p.is_file()) {
        std::fs::copy(cfg, home.join("config.toml"))
            .map_err(|e| format!("copying {}: {e}", cfg.display()))?;
    }
    let write_script = |exports: &[(String, String)], patch_out: &str| {
        std::fs::write(
            copy.join(SANDBOX_DIR).join("run.sh"),
            guest_script(worker_args, exports, patch_out),
        )
        .map_err(|e| e.to_string())
    };

    match sb.tier {
        IsolationTier::Container => {
            write_script(&[], &format!("{GUEST_WORK}/{SANDBOX_DIR}/patch"))?;
            run.args = container_worker_argv(&sb.config, &copy, &id, owner_of(&copy));
            run.program = "docker".into();
            run.container = Some(id);
        }
        IsolationTier::Vm => {
            // A VM has no env passthrough, so forwarded values are written
            // into the script on the worktree drive, which is deleted with the
            // scratch when the task ends.
            let exports: Vec<(String, String)> = sb
                .config
                .env
                .iter()
                .filter(|k| is_env_name(k))
                .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
                .collect();
            write_script(&exports, "/dev/vdc")?;
            let vm = &sb.config.vm;
            let dir = if vm.use_jailer {
                let exec = Path::new(&vm.firecracker_bin)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "firecracker".into());
                let jail = Path::new(&vm.chroot_base_dir).join(exec).join(&id);
                let _ = std::fs::remove_dir_all(&jail);
                run.jail = Some(jail.clone());
                jail.join("root")
            } else {
                scratch.clone()
            };
            std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
            let work = dir.join("work.ext4");
            let size = format!("{}M", vm.worktree_drive_mib);
            let (copy_s, work_s) = (copy.to_string_lossy(), work.to_string_lossy());
            let out = runner
                .run(
                    "mke2fs",
                    &["-q", "-F", "-t", "ext4", "-d", &copy_s, &work_s, &size],
                    &scratch,
                )
                .map_err(|e| format!("mke2fs: {e}"))?;
            if !out.success() {
                return Err(format!(
                    "mke2fs could not pack the worktree into {size}: {}",
                    out.stderr.trim()
                ));
            }
            let patch = dir.join("patch.img");
            std::fs::File::create(&patch)
                .and_then(|f| f.set_len(PATCH_DRIVE_BYTES))
                .map_err(|e| format!("creating the patch drive: {e}"))?;
            run.patch_drive = Some(patch.clone());

            let config = dir.join("config.json");
            let body = if vm.use_jailer {
                stage(Path::new(&vm.kernel_image), &dir.join("vmlinux"))?;
                stage(Path::new(&vm.rootfs_image), &dir.join("rootfs.ext4"))?;
                firecracker_config(
                    &sb.config,
                    "vmlinux",
                    "rootfs.ext4",
                    "work.ext4",
                    "patch.img",
                )
            } else {
                firecracker_config(
                    &sb.config,
                    &vm.kernel_image,
                    &vm.rootfs_image,
                    &work_s,
                    &patch.to_string_lossy(),
                )
            };
            std::fs::write(&config, body.to_string()).map_err(|e| e.to_string())?;

            if vm.use_jailer {
                // Firecracker runs as jailer_uid inside the chroot and must be
                // able to write both drives.
                #[cfg(unix)]
                for f in [&work, &patch, &config] {
                    std::os::unix::fs::chown(f, Some(vm.jailer_uid), Some(vm.jailer_gid))
                        .map_err(|e| format!("chown {}: {e}", f.display()))?;
                }
                run.program = vm.jailer_bin.clone();
                run.args = jailer_argv(&sb.config, &id);
            } else {
                run.program = vm.firecracker_bin.clone();
                run.args = firecracker_argv(&config.to_string_lossy());
            }
        }
        IsolationTier::Host => return Err("the host tier has no sandbox to prepare".into()),
    }
    Ok(run)
}

/// Bring a finished sandbox's changes back: pull its patch out and `git
/// apply` it to `worktree`. Returns whether anything changed; the caller
/// commits.
///
/// The patch is untrusted. A container's copy is checked for links before it
/// is read (the container could have planted one pointing at a host file); a
/// vm's patch comes off a raw drive, so no guest filesystem is ever parsed on
/// the host.
pub fn patch_back(
    run: &SandboxRun,
    worktree: &Path,
    runner: &dyn CommandRunner,
) -> Result<bool, String> {
    let body = if run.tier == IsolationTier::Vm {
        let drive = run
            .patch_drive
            .as_deref()
            .ok_or("vm run has no patch drive")?;
        let mut raw = Vec::new();
        std::fs::File::open(drive)
            .and_then(|f| f.take(PATCH_DRIVE_BYTES).read_to_end(&mut raw))
            .map_err(|e| format!("reading the patch drive: {e}"))?;
        // A diff never contains NUL (binary hunks are base85), so the patch
        // ends where the zeroed drive begins.
        let Some(end) = raw.iter().position(|b| *b == 0) else {
            return Err("the vm patch filled its output drive; the diff is too large".into());
        };
        raw.truncate(end);
        raw
    } else {
        let dir = run.copy.join(SANDBOX_DIR);
        let src = dir.join("patch");
        let plain_dir = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir());
        let plain_file = std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file());
        if !(plain_dir && plain_file) {
            return Err("the sandbox left no patch file (or replaced it with a link)".into());
        }
        std::fs::read(&src).map_err(|e| format!("reading the patch: {e}"))?
    };
    let Some(diff) = body.strip_suffix(PATCH_END.as_bytes()) else {
        return Err("the sandbox did not finish writing its patch".into());
    };
    if diff.is_empty() {
        return Ok(false);
    }
    let patch = run.scratch.join("patch");
    std::fs::write(&patch, diff).map_err(|e| e.to_string())?;
    let patch_s = patch.to_string_lossy();
    // The guest script leaves Wingman's own trees out of the diff, but the
    // worker controls the guest and can force-add them: `.wingman-sandbox/`
    // holds the copied global config (provider keys), which would otherwise be
    // committed to the task branch. Enforced here, where the patch lands.
    let exclude_sandbox = format!("--exclude={SANDBOX_DIR}/*");
    let out = runner
        .run(
            "git",
            &[
                "apply",
                "--binary",
                "--whitespace=nowarn",
                &exclude_sandbox,
                "--exclude=.wingman/*",
                &patch_s,
            ],
            worktree,
        )
        .map_err(|e| format!("git apply: {e}"))?;
    if !out.success() {
        return Err(format!(
            "git apply rejected the sandbox patch: {}",
            out.stderr.trim()
        ));
    }
    Ok(true)
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

    /// Records every command and succeeds, except `fail` (which exits 1) and
    /// `id -u`, which answers `uid`.
    struct Fake {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        fail: Option<String>,
        uid: &'static str,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: None,
                uid: "0",
            }
        }
        fn failing(bin: &str) -> Self {
            Self {
                fail: Some(bin.to_string()),
                ..Self::new()
            }
        }
        fn called(&self, program: &str) -> Option<Vec<String>> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .find(|(p, _)| p == program)
                .map(|(_, a)| a.clone())
        }
    }
    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[&str], _cwd: &Path) -> std::io::Result<CommandOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let ok = self.fail.as_deref() != Some(program);
            // Like the real mke2fs, leave the image behind: the jailer path
            // chowns it on Unix, which fails on a file that was never made.
            if ok && program == "mke2fs" && args.contains(&"-d") {
                std::fs::File::create(args[args.len() - 2])?;
            }
            Ok(CommandOut {
                status: Some(if ok { 0 } else { 1 }),
                stdout: if program == "id" {
                    self.uid.into()
                } else {
                    String::new()
                },
                stderr: if ok { String::new() } else { "boom".into() },
            })
        }
    }

    fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag).unwrap();
        &argv[i + 1]
    }

    #[test]
    fn docker_available_reflects_daemon() {
        assert!(docker_available(&Fake::new()));
        assert!(!docker_available(&Fake::failing("docker")));
    }

    fn avail(docker: bool, vm: bool) -> TierAvailability {
        TierAvailability {
            docker,
            vm: if vm { Ok(()) } else { Err("no kvm".into()) },
        }
    }

    #[test]
    fn host_tier_never_degrades() {
        assert_eq!(
            resolve_effective_tier(IsolationTier::Host, &avail(false, false)),
            (IsolationTier::Host, false)
        );
    }

    #[test]
    fn container_tier_needs_docker() {
        assert_eq!(
            resolve_effective_tier(IsolationTier::Container, &avail(true, false)),
            (IsolationTier::Container, false)
        );
        assert_eq!(
            resolve_effective_tier(IsolationTier::Container, &avail(false, true)),
            (IsolationTier::Host, true)
        );
    }

    #[test]
    fn vm_tier_needs_a_vm_backend_not_docker() {
        assert_eq!(
            resolve_effective_tier(IsolationTier::Vm, &avail(false, true)),
            (IsolationTier::Vm, false)
        );
        // Docker alone does not make a vm: it takes the best tier left, and
        // says so.
        assert_eq!(
            resolve_effective_tier(IsolationTier::Vm, &avail(true, false)),
            (IsolationTier::Container, true)
        );
        assert_eq!(
            resolve_effective_tier(IsolationTier::Vm, &avail(false, false)),
            (IsolationTier::Host, true)
        );
    }

    /// A config whose vm tier is complete, with images under `dir`.
    fn vm_cfg(dir: &Path) -> PilotSandboxConfig {
        let mut cfg = PilotSandboxConfig::default();
        for f in ["vmlinux", "rootfs.ext4", "kvm"] {
            std::fs::write(dir.join(f), b"img").unwrap();
        }
        cfg.vm.kernel_image = dir.join("vmlinux").to_string_lossy().into_owned();
        cfg.vm.rootfs_image = dir.join("rootfs.ext4").to_string_lossy().into_owned();
        cfg.vm.firecracker_bin = if cfg!(windows) {
            r"C:\fc\firecracker".into()
        } else {
            "/usr/bin/firecracker".into()
        };
        cfg
    }

    #[test]
    fn vm_available_only_with_every_piece() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = vm_cfg(dir.path());
        let kvm = dir.path().join("kvm");
        assert_eq!(vm_available(&cfg, &Fake::new(), "linux", &kvm), Ok(()));

        let reason = |cfg: &PilotSandboxConfig, runner: &Fake, os: &str, kvm: &Path| {
            vm_available(cfg, runner, os, kvm).unwrap_err()
        };
        assert!(reason(&cfg, &Fake::new(), "windows", &kvm).contains("needs Linux"));
        assert!(reason(&cfg, &Fake::new(), "macos", &kvm).contains("needs Linux"));
        assert!(
            reason(&cfg, &Fake::new(), "linux", &dir.path().join("nokvm")).contains("not usable")
        );

        let mut qemu = cfg.clone();
        qemu.vm_provider = "qemu".into();
        assert!(reason(&qemu, &Fake::new(), "linux", &kvm).contains("not implemented"));

        let mut no_kernel = cfg.clone();
        no_kernel.vm.kernel_image.clear();
        assert!(reason(&no_kernel, &Fake::new(), "linux", &kvm).contains("kernel_image"));
        let mut gone = cfg.clone();
        gone.vm.rootfs_image = dir.path().join("nope").to_string_lossy().into_owned();
        assert!(reason(&gone, &Fake::new(), "linux", &kvm).contains("does not exist"));

        assert!(
            reason(&cfg, &Fake::failing(&cfg.vm.firecracker_bin), "linux", &kvm)
                .contains("not runnable")
        );
        assert!(reason(&cfg, &Fake::failing("mke2fs"), "linux", &kvm).contains("mke2fs"));
        assert!(reason(&cfg, &Fake::failing("jailer"), "linux", &kvm).contains("jailer"));

        let not_root = Fake {
            uid: "1000",
            ..Fake::new()
        };
        assert!(reason(&cfg, &not_root, "linux", &kvm).contains("needs root"));
        let mut relative = cfg.clone();
        relative.vm.firecracker_bin = "firecracker".into();
        assert!(reason(&relative, &Fake::new(), "linux", &kvm).contains("absolute"));

        // Without the jailer, neither root nor an absolute path is required.
        let mut unjailed = relative.clone();
        unjailed.vm.use_jailer = false;
        assert_eq!(vm_available(&unjailed, &not_root, "linux", &kvm), Ok(()));
    }

    #[test]
    fn container_argv_applies_limits_network_and_env() {
        let cfg = PilotSandboxConfig {
            network: "none".into(),
            cpus: 3,
            memory_mib: 1024,
            pids_limit: 99,
            env: vec!["ANTHROPIC_API_KEY".into(), "LEAK=value".into()],
            ..Default::default()
        };
        let argv =
            container_worker_argv(&cfg, Path::new("/tmp/copy"), "wingman-x", Some((1000, 50)));
        assert!(!argv.iter().any(|a| a.contains("value")));
        assert_eq!(&argv[..3], ["run", "--rm", "-i"]);
        assert_eq!(value_after(&argv, "--name"), "wingman-x");
        assert_eq!(value_after(&argv, "--network"), "none");
        assert_eq!(value_after(&argv, "--cpus"), "3");
        assert_eq!(value_after(&argv, "--memory"), "1024m");
        assert_eq!(value_after(&argv, "--pids-limit"), "99");
        assert_eq!(value_after(&argv, "--security-opt"), "no-new-privileges");
        assert_eq!(value_after(&argv, "--user"), "1000:50");
        // By name only: the value stays out of argv.
        assert_eq!(value_after(&argv, "-e"), "ANTHROPIC_API_KEY");
        assert_eq!(value_after(&argv, "-v"), "/tmp/copy:/work");
        assert_eq!(value_after(&argv, "-w"), "/work");
        assert_eq!(
            &argv[argv.len() - 3..],
            [
                "wingman/sandbox:latest",
                "sh",
                "/work/.wingman-sandbox/run.sh"
            ]
        );
        let no_user = container_worker_argv(&cfg, Path::new("/c"), "n", None);
        assert!(!no_user.iter().any(|a| a == "--user"));
    }

    #[test]
    fn guest_script_snapshots_runs_and_diffs() {
        let s = guest_script(
            &["--role".into(), "it's".into()],
            &[("KEY".into(), "s3cr'et".into())],
            "/dev/vdc",
        );
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("cd /work || exit 97"));
        assert!(s.contains("export KEY='s3cr'\\''et'"));
        assert!(s.contains("/.wingman-sandbox/"));
        assert!(s.contains("git commit -q --allow-empty -m 'sandbox base' || exit 97"));
        assert!(s.contains("wingman '--role' 'it'\\''s'\nrc=$?"));
        assert!(s.contains("GIT_CONFIG_KEY_0=safe.directory"));
        assert!(s.contains("git diff --cached --binary \"$base\" > /tmp/wingman.patch"));
        assert!(s.contains(
            "printf '# wingman-sandbox: patch complete\\n' >> /tmp/wingman.patch \
             && cat /tmp/wingman.patch > '/dev/vdc' && sync || exit 98"
        ));
        assert!(s.trim_end().ends_with("exit $rc"));
    }

    #[test]
    fn firecracker_config_has_drives_limits_and_optional_tap() {
        let mut cfg = PilotSandboxConfig {
            cpus: 4,
            memory_mib: 2048,
            ..Default::default()
        };
        let v = firecracker_config(&cfg, "vmlinux", "rootfs.ext4", "work.ext4", "patch.img");
        assert_eq!(v["boot-source"]["kernel_image_path"], "vmlinux");
        let boot = v["boot-source"]["boot_args"].as_str().unwrap();
        assert!(boot.contains("init=/sbin/wingman-sandbox-init"));
        assert!(boot.contains("console=ttyS0"));
        let drives = v["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 3);
        assert_eq!(drives[0]["path_on_host"], "rootfs.ext4");
        assert_eq!(drives[0]["is_root_device"], true);
        assert_eq!(drives[0]["is_read_only"], true);
        assert_eq!(drives[1]["path_on_host"], "work.ext4");
        assert_eq!(drives[1]["is_read_only"], false);
        assert_eq!(drives[2]["path_on_host"], "patch.img");
        assert_eq!(v["machine-config"]["vcpu_count"], 4);
        assert_eq!(v["machine-config"]["mem_size_mib"], 2048);
        assert!(v.get("network-interfaces").is_none());

        cfg.vm.tap_device = "tap0".into();
        let v = firecracker_config(&cfg, "k", "r", "w", "p");
        assert_eq!(v["network-interfaces"][0]["host_dev_name"], "tap0");
    }

    #[test]
    fn jailer_argv_drops_privileges_into_the_chroot() {
        let mut cfg = PilotSandboxConfig::default();
        cfg.vm.firecracker_bin = "/usr/bin/firecracker".into();
        cfg.vm.jailer_uid = 123;
        cfg.vm.jailer_gid = 456;
        let argv = jailer_argv(&cfg, "wingman-t1");
        assert_eq!(value_after(&argv, "--id"), "wingman-t1");
        assert_eq!(value_after(&argv, "--exec-file"), "/usr/bin/firecracker");
        assert_eq!(value_after(&argv, "--uid"), "123");
        assert_eq!(value_after(&argv, "--gid"), "456");
        assert_eq!(value_after(&argv, "--chroot-base-dir"), "/srv/jailer");
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &argv[sep + 1..],
            ["--no-api", "--config-file", "config.json"]
        );
    }

    #[test]
    fn sandbox_id_is_a_valid_container_name_and_jailer_id() {
        let id = sandbox_id(&format!("pilot-run/1:agent_{}", "x".repeat(100)));
        assert!(id.len() <= 64);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    /// A git repo with one committed file, standing in for a task worktree.
    fn worktree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "seed"]);
        dir
    }

    fn sandbox(tier: IsolationTier, cfg: PilotSandboxConfig) -> WorkerSandbox {
        WorkerSandbox {
            tier,
            config: cfg,
            global_config: None,
        }
    }

    #[test]
    fn prepare_container_copies_the_worktree_without_git() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let global = home.path().join("config.toml");
        std::fs::write(&global, "default_model = \"x\"\n").unwrap();
        let mut sb = sandbox(IsolationTier::Container, PilotSandboxConfig::default());
        sb.global_config = Some(global);
        let runner = Fake::new();
        let args = vec!["--worker-mode".to_string()];
        let run = prepare(&sb, wt.path(), "pilot-r-a1", &args, &runner, scratch.path()).unwrap();

        assert_eq!(run.program, "docker");
        assert!(runner.calls.lock().unwrap().is_empty());
        let copy = scratch.path().join("wingman-pilot-r-a1").join("work");
        assert_eq!(
            value_after(&run.args, "-v"),
            format!("{}:/work", copy.display())
        );
        assert_eq!(
            std::fs::read_to_string(copy.join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(!copy.join(".git").exists());
        let script = std::fs::read_to_string(copy.join(".wingman-sandbox/run.sh")).unwrap();
        assert!(script.contains("> '/work/.wingman-sandbox/patch'"));
        assert!(script.contains("wingman '--worker-mode'"));
        assert!(copy.join(".wingman-sandbox/home/config.toml").is_file());

        drop(run);
        assert!(!scratch.path().join("wingman-pilot-r-a1").exists());
    }

    #[test]
    fn prepare_vm_packs_the_drive_and_points_firecracker_at_it() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let mut cfg = vm_cfg(scratch.path());
        cfg.vm.use_jailer = false;
        cfg.vm.worktree_drive_mib = 512;
        let runner = Fake::new();
        let run = prepare(
            &sandbox(IsolationTier::Vm, cfg.clone()),
            wt.path(),
            "s1",
            &[],
            &runner,
            scratch.path(),
        )
        .unwrap();

        let root = scratch.path().join("wingman-s1");
        let mke2fs = runner.called("mke2fs").unwrap();
        assert_eq!(
            value_after(&mke2fs, "-d"),
            root.join("work").to_string_lossy()
        );
        assert_eq!(value_after(&mke2fs, "-t"), "ext4");
        assert_eq!(mke2fs.last().unwrap(), "512M");
        assert_eq!(run.program, cfg.vm.firecracker_bin);
        assert_eq!(&run.args[..2], ["--no-api", "--config-file"]);
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&run.args[2]).unwrap()).unwrap();
        assert_eq!(
            config["boot-source"]["kernel_image_path"],
            cfg.vm.kernel_image
        );
        assert_eq!(config["drives"][0]["path_on_host"], cfg.vm.rootfs_image);
        assert_eq!(
            config["drives"][1]["path_on_host"],
            root.join("work.ext4").to_string_lossy().as_ref()
        );
        let patch = root.join("patch.img");
        assert_eq!(std::fs::metadata(&patch).unwrap().len(), PATCH_DRIVE_BYTES);
        let script = std::fs::read_to_string(root.join("work/.wingman-sandbox/run.sh")).unwrap();
        assert!(script.contains("> '/dev/vdc'"));
    }

    #[test]
    fn prepare_vm_stages_the_jail() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let mut cfg = vm_cfg(scratch.path());
        cfg.vm.chroot_base_dir = base.path().to_string_lossy().into_owned();
        // chown to ourselves, so the test needs no root.
        if let Some((uid, gid)) = owner_of(base.path()) {
            cfg.vm.jailer_uid = uid;
            cfg.vm.jailer_gid = gid;
        }
        let run = prepare(
            &sandbox(IsolationTier::Vm, cfg.clone()),
            wt.path(),
            "s2",
            &[],
            &Fake::new(),
            scratch.path(),
        )
        .unwrap();

        assert_eq!(run.program, "jailer");
        assert_eq!(run.args, jailer_argv(&cfg, "wingman-s2"));
        let jail = base.path().join("firecracker").join("wingman-s2");
        let root = jail.join("root");
        for f in ["vmlinux", "rootfs.ext4", "patch.img", "config.json"] {
            assert!(root.join(f).is_file(), "{f} staged");
        }
        // Inside the chroot, paths are relative to its root.
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config["boot-source"]["kernel_image_path"], "vmlinux");
        assert_eq!(config["drives"][1]["path_on_host"], "work.ext4");

        drop(run);
        assert!(!jail.exists());
    }

    #[test]
    fn prepare_fails_when_the_worktree_does_not_fit() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let mut cfg = vm_cfg(scratch.path());
        cfg.vm.use_jailer = false;
        let err = prepare(
            &sandbox(IsolationTier::Vm, cfg),
            wt.path(),
            "s3",
            &[],
            &Fake::failing("mke2fs"),
            scratch.path(),
        )
        .unwrap_err();
        assert!(err.contains("mke2fs could not pack"), "{err}");
        // The partial scratch went with the error.
        assert!(!scratch.path().join("wingman-s3").exists());
    }

    /// Run git in `dir` the way the guest script does, returning stdout.
    fn guest_git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "w")
            .env("GIT_AUTHOR_EMAIL", "w@w")
            .env("GIT_COMMITTER_NAME", "w")
            .env("GIT_COMMITTER_EMAIL", "w@w")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out.stdout
    }

    /// Stand in for the sandbox: in the copy, make the base commit, edit,
    /// commit some of it, and emit the diff the script would.
    fn simulate_worker(copy: &Path) -> Vec<u8> {
        guest_git(copy, &["init", "-q"]);
        guest_git(copy, &["config", "core.autocrlf", "false"]);
        std::fs::write(copy.join(".git/info/exclude"), "/.wingman-sandbox/\n").unwrap();
        guest_git(copy, &["add", "-A"]);
        guest_git(copy, &["commit", "-qm", "sandbox base"]);
        let base = String::from_utf8(guest_git(copy, &["rev-parse", "HEAD"])).unwrap();
        std::fs::write(copy.join("a.txt"), "one\ntwo\n").unwrap();
        guest_git(copy, &["commit", "-qam", "worker commit"]);
        std::fs::write(copy.join("new.bin"), [0u8, 1, 2, 255]).unwrap();
        guest_git(copy, &["add", "-A"]);
        // A hostile worker force-adds Wingman's own trees past the exclude.
        std::fs::create_dir_all(copy.join(".wingman/deep")).unwrap();
        std::fs::write(copy.join(".wingman/deep/leak"), "x").unwrap();
        std::fs::write(copy.join(".wingman-sandbox/home/config.toml"), "k").unwrap();
        guest_git(copy, &["add", "-f", ".wingman-sandbox", ".wingman"]);
        let mut diff = guest_git(copy, &["diff", "--cached", "--binary", base.trim()]);
        diff.extend_from_slice(PATCH_END.as_bytes());
        diff
    }

    #[test]
    fn container_patch_back_applies_committed_and_uncommitted_work() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let sb = sandbox(IsolationTier::Container, PilotSandboxConfig::default());
        let run = prepare(&sb, wt.path(), "p1", &[], &Fake::new(), scratch.path()).unwrap();
        let patch = simulate_worker(&run.copy);
        std::fs::write(run.copy.join(".wingman-sandbox/patch"), patch).unwrap();

        let changed = patch_back(&run, wt.path(), &crate::pr::SystemCommandRunner).unwrap();
        assert!(changed);
        assert_eq!(
            std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            std::fs::read(wt.path().join("new.bin")).unwrap(),
            [0u8, 1, 2, 255]
        );
        // Wingman's own files never come back, even when force-added.
        assert!(!wt.path().join(".wingman-sandbox").exists());
        assert!(!wt.path().join(".wingman").exists());
    }

    #[test]
    fn vm_patch_back_reads_the_raw_drive() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let mut cfg = vm_cfg(scratch.path());
        cfg.vm.use_jailer = false;
        let run = prepare(
            &sandbox(IsolationTier::Vm, cfg),
            wt.path(),
            "p2",
            &[],
            &Fake::new(),
            scratch.path(),
        )
        .unwrap();
        // The copy is what the guest saw; the patch goes to the start of the
        // zeroed drive, as `> /dev/vdc` would put it.
        let patch = simulate_worker(&run.copy);
        {
            use std::io::{Seek as _, Write as _};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(run.patch_drive.as_ref().unwrap())
                .unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(&patch).unwrap();
        }
        assert!(patch_back(&run, wt.path(), &crate::pr::SystemCommandRunner).unwrap());
        assert_eq!(
            std::fs::read_to_string(wt.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );

        // A drive with no terminating NUL means the diff was cut off.
        std::fs::write(run.patch_drive.as_ref().unwrap(), b"diff --git").unwrap();
        let err = patch_back(&run, wt.path(), &Fake::new()).unwrap_err();
        assert!(err.contains("filled its output drive"), "{err}");
    }

    #[test]
    fn empty_patch_changes_nothing() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let sb = sandbox(IsolationTier::Container, PilotSandboxConfig::default());
        let run = prepare(&sb, wt.path(), "p3", &[], &Fake::new(), scratch.path()).unwrap();
        std::fs::write(run.copy.join(".wingman-sandbox/patch"), PATCH_END).unwrap();
        let runner = Fake::new();
        assert_eq!(patch_back(&run, wt.path(), &runner), Ok(false));
        assert!(runner.called("git").is_none());
    }

    #[test]
    fn patch_back_refuses_a_missing_or_rejected_patch() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let sb = sandbox(IsolationTier::Container, PilotSandboxConfig::default());
        let run = prepare(&sb, wt.path(), "p4", &[], &Fake::new(), scratch.path()).unwrap();
        let err = patch_back(&run, wt.path(), &Fake::new()).unwrap_err();
        assert!(err.contains("no patch file"), "{err}");

        // Cut short: no completion line, so nothing is applied.
        let patch_file = run.copy.join(".wingman-sandbox/patch");
        std::fs::write(&patch_file, "diff --git a/x b/x\n").unwrap();
        let runner = Fake::new();
        let err = patch_back(&run, wt.path(), &runner).unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        assert!(runner.called("git").is_none());

        std::fs::write(&patch_file, format!("diff --git a/x b/x\n{PATCH_END}")).unwrap();
        let runner = Fake::failing("git");
        let err = patch_back(&run, wt.path(), &runner).unwrap_err();
        assert!(err.contains("git apply rejected"), "{err}");
        let args = runner.called("git").unwrap();
        assert_eq!(&args[..3], ["apply", "--binary", "--whitespace=nowarn"]);
    }

    #[cfg(unix)]
    #[test]
    fn patch_back_will_not_follow_a_planted_link() {
        let wt = worktree();
        let scratch = tempfile::tempdir().unwrap();
        let sb = sandbox(IsolationTier::Container, PilotSandboxConfig::default());
        let run = prepare(&sb, wt.path(), "p5", &[], &Fake::new(), scratch.path()).unwrap();
        let secret = scratch.path().join("secret");
        std::fs::write(&secret, "host credentials").unwrap();
        std::os::unix::fs::symlink(&secret, run.copy.join(".wingman-sandbox/patch")).unwrap();
        let runner = Fake::new();
        assert!(patch_back(&run, wt.path(), &runner).is_err());
        assert!(runner.called("git").is_none());
    }

    #[test]
    fn highest_signal_wins() {
        // Both a dep change (container) and a migration (vm) → vm.
        let t = task(&["Cargo.toml", "migrations/x.sql"], Reversibility::Trivial);
        assert_eq!(select_tier(&t, IsolationTier::Host), IsolationTier::Vm);
    }
}
