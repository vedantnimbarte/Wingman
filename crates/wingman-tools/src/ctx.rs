use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use wingman_config::PermissionMode;

#[derive(Clone)]
pub struct ToolCtx {
    /// Permission mode, held behind an atomic so a live session (e.g. the
    /// TUI `/mode` picker) can re-gate the running agent's tools without
    /// rebuilding the registry. Cloning a `ToolCtx` shares the same cell, so
    /// a `set_mode` on any handle flips enforcement everywhere it's shared.
    mode: Arc<AtomicU8>,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// Extra shell command patterns that are always denied, even in yolo mode.
    /// Each entry is a substring pattern: if the command contains it, the call
    /// is rejected before execution.
    pub extra_denylist: Vec<String>,
    /// Opt-in from `[tools].allow_network`: permit web_fetch/web_search in any
    /// mode (including read-only/plan), not just the edit-capable ones. Off by
    /// default so network egress stays gated unless the user asks for it.
    pub allow_network: bool,
    /// `[tools].shell_sandbox`: `auto` | `off` | `required`. See
    /// [`crate::sandbox`].
    pub shell_sandbox: String,
    /// Has the user approved a plan this session? Only meaningful in
    /// [`PermissionMode::Plan`], where writes and shell stay denied until the
    /// agent has presented a plan *and* the user accepted it. Shared like
    /// `mode`, so approving once re-gates every clone.
    plan_approved: Arc<AtomicBool>,
    /// Background shell jobs for this session.
    ///
    /// Shared across clones — deliberately, like `mode`. A subagent inherits
    /// the parent's `ToolCtx`, so a job it starts belongs to the session
    /// rather than to a child that is about to disappear; giving children
    /// their own table would orphan whatever they left running.
    pub jobs: Arc<crate::jobs::JobTable>,
    /// How tools read and write files.
    ///
    /// Shared across clones like  and . Note this is a tool's
    /// *content* I/O only — path containment below deliberately consults the
    /// real filesystem whatever this is set to, because whether a path escapes
    /// the project is a fact about the real one.
    pub fs: Arc<dyn crate::filesystem::FileSystem>,
}

/// Encode/decode `PermissionMode` as a `u8` for the atomic cell. Kept local
/// so the config enum stays free of storage concerns; an unknown byte
/// decodes to the safe default (`ReadOnly`).
fn mode_to_u8(m: PermissionMode) -> u8 {
    match m {
        PermissionMode::ReadOnly => 0,
        PermissionMode::Plan => 1,
        PermissionMode::AutoEdit => 2,
        PermissionMode::Yolo => 3,
    }
}

fn mode_from_u8(b: u8) -> PermissionMode {
    match b {
        1 => PermissionMode::Plan,
        2 => PermissionMode::AutoEdit,
        3 => PermissionMode::Yolo,
        _ => PermissionMode::ReadOnly,
    }
}

impl ToolCtx {
    pub fn new(mode: PermissionMode, cwd: PathBuf, project_root: PathBuf) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode_to_u8(mode))),
            cwd,
            project_root,
            extra_denylist: Vec::new(),
            allow_network: false,
            shell_sandbox: "auto".into(),
            plan_approved: Arc::new(AtomicBool::new(false)),
            jobs: Arc::new(crate::jobs::JobTable::new()),
            fs: Arc::new(crate::filesystem::OsFileSystem),
        }
    }

    /// Like [`new`] but also accepts a project-level denylist of shell patterns
    /// and the `allow_network` opt-in.
    pub fn new_with_config(
        mode: PermissionMode,
        cwd: PathBuf,
        project_root: PathBuf,
        extra_denylist: Vec<String>,
        allow_network: bool,
    ) -> Self {
        Self {
            mode: Arc::new(AtomicU8::new(mode_to_u8(mode))),
            cwd,
            project_root,
            extra_denylist,
            allow_network,
            shell_sandbox: "auto".into(),
            plan_approved: Arc::new(AtomicBool::new(false)),
            jobs: Arc::new(crate::jobs::JobTable::new()),
            fs: Arc::new(crate::filesystem::OsFileSystem),
        }
    }

    /// Current permission mode.
    pub fn mode(&self) -> PermissionMode {
        mode_from_u8(self.mode.load(Ordering::SeqCst))
    }

    /// Switch the permission mode live. Shared across clones of this ctx, so
    /// the running agent's next tool call is gated by the new mode.
    ///
    /// Switching *out* of `Plan` clears any plan approval, so returning to
    /// plan mode later starts from "no plan approved" rather than inheriting
    /// consent given for different work.
    pub fn set_mode(&self, mode: PermissionMode) {
        if mode != PermissionMode::Plan {
            self.plan_approved.store(false, Ordering::SeqCst);
        }
        self.mode.store(mode_to_u8(mode), Ordering::SeqCst);
    }

    /// Set the shell-sandbox policy (`[tools].shell_sandbox`). Builder-style.
    pub fn with_shell_sandbox(mut self, policy: impl Into<String>) -> Self {
        self.shell_sandbox = policy.into();
        self
    }

    /// Record that the user accepted the agent's plan.
    ///
    /// This is what makes `plan` mode a gate rather than a naming convention:
    /// until it is called, `plan` denies writes and shell exactly like
    /// `read-only`; afterwards it behaves like `auto-edit` for the rest of the
    /// session. Only a human action should call this — never a tool.
    pub fn approve_plan(&self) {
        self.plan_approved.store(true, Ordering::SeqCst);
    }

    /// Has a plan been approved this session?
    pub fn plan_approved(&self) -> bool {
        self.plan_approved.load(Ordering::SeqCst)
    }

    /// Returns `true` if `command` matches any entry in the project-level
    /// denylist. Always-deny takes precedence over the permission mode.
    ///
    /// Matching is argv-based, not substring-based. The command is split on
    /// unquoted shell operators (`;`, `&&`, `||`, `|`, newline) into
    /// sub-commands and each is tokenized quote/escape-aware. A pattern
    /// matches when:
    ///   - it is a single token equal to a sub-command's program (by
    ///     basename, so `sudo` blocks `/usr/bin/sudo` and `foo && sudo bar`)
    ///     or to any standalone argument token; or
    ///   - it is multiple tokens appearing as a contiguous run in some
    ///     sub-command's argv.
    ///
    /// This closes the trivial whitespace / `&&`-chaining / substring
    /// bypasses of the old `command.contains(pattern)` check. It is still a
    /// denylist (best-effort): it cannot see through `eval`, base64, or env
    /// indirection. For a hard boundary, run untrusted commands in a sandbox
    /// tier rather than relying on this filter.
    pub fn is_shell_denied(&self, command: &str) -> bool {
        if self.extra_denylist.is_empty() {
            return false;
        }
        let cmds = tokenize_shell(command);
        for pattern in &self.extra_denylist {
            let pat_tokens: Vec<String> = tokenize_shell(pattern).into_iter().flatten().collect();
            if pat_tokens.is_empty() {
                continue;
            }
            if cmds
                .iter()
                .any(|cmd| command_matches_pattern(cmd, &pat_tokens))
            {
                return true;
            }
        }
        false
    }

    /// Resolve a tool-supplied path against the cwd. Returns canonicalized
    /// form when possible, but accepts non-existent paths too (callers may
    /// be about to create them).
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = PathBuf::from(p);
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    pub fn is_inside_project(&self, path: &Path) -> bool {
        let root =
            std::fs::canonicalize(&self.project_root).unwrap_or_else(|_| self.project_root.clone());
        let resolved = resolve_for_containment(path);
        resolved.starts_with(&root)
    }

    /// Permission for a write/edit operation on `path`.
    ///
    /// Protected paths (see [`Self::is_protected_path`]) are refused in *every*
    /// mode, `yolo` included — those are the files that turn a single bad edit
    /// into a permanent compromise.
    pub fn allows_write(&self, path: &Path) -> bool {
        if self.is_protected_path(path) {
            return false;
        }
        match self.mode() {
            PermissionMode::Yolo => true,
            PermissionMode::AutoEdit => self.is_inside_project(path),
            // Plan is read-only until the user accepts a plan, then behaves
            // like auto-edit. Before this, `plan` was byte-identical to
            // read-only and `present_plan` gated nothing.
            PermissionMode::Plan => self.plan_approved() && self.is_inside_project(path),
            PermissionMode::ReadOnly => false,
        }
    }

    /// Why a write to `path` was refused, phrased for the model.
    ///
    /// Distinguishes "this mode forbids writing" from "this file is protected
    /// in every mode", because the remedy differs — switching to `yolo` will
    /// not unlock a protected path, and telling the model otherwise just makes
    /// it retry.
    pub fn write_denial_reason(&self, path: &Path) -> String {
        if self.is_protected_path(path) {
            format!(
                "write denied for {}: this path is protected in every mode, \
                 including yolo (writing it would let a change persist beyond \
                 this session). Edit it yourself if you meant to.",
                path.display()
            )
        } else {
            format!(
                "write denied for {} under permission mode {}",
                path.display(),
                self.mode()
            )
        }
    }

    /// Files the agent must never write, regardless of permission mode.
    ///
    /// Each of these converts one successful edit into durable, out-of-band
    /// code execution or a permission escalation that survives the session:
    ///
    ///   - `.git/` — a `hooks/pre-commit` fires on the developer's next commit,
    ///     entirely outside Wingman; `config` can point `origin` elsewhere.
    ///   - `.wingman/config.toml` — the agent could grant itself `yolo`, add
    ///     `[hooks]`, or redirect a provider `base_url` (see the config trust
    ///     boundary in `wingman_config`).
    ///   - `.wingman/skills/` — skills are injected into the system prompt and
    ///     returned with an instruction to obey them.
    ///   - `.wingman/trusted.toml` — the record of what the *user* trusted.
    ///
    /// `yolo` is documented as "no guardrails" for the agent's own work; it is
    /// not an invitation to rewrite the mechanism that enforces the guardrails.
    /// A user who genuinely needs to change these can edit them directly.
    pub fn is_protected_path(&self, path: &Path) -> bool {
        let resolved = resolve_for_containment(path);
        let root =
            std::fs::canonicalize(&self.project_root).unwrap_or_else(|_| self.project_root.clone());
        let Ok(rel) = resolved.strip_prefix(&root) else {
            // Outside the project: mode checks already govern it, and there is
            // no project-relative protected path to match.
            return false;
        };

        let mut comps = rel.components().map(|c| c.as_os_str().to_string_lossy());
        match comps.next().as_deref() {
            Some(".git") => true,
            Some(".wingman") => matches!(
                comps.next().as_deref(),
                Some("config.toml") | Some("skills") | Some("trusted.toml")
            ),
            _ => false,
        }
    }

    /// Permission for a read of `path`.
    ///
    /// Reads are confined to the project tree in every mode except `Yolo`, so
    /// the agent (or prompt-injected instructions in content it reads) can't
    /// pull `~/.ssh/id_rsa`, `~/.aws/credentials`, etc. into tool output and
    /// exfiltrate them. `Yolo` is the explicit "no guardrails" escape hatch.
    pub fn allows_read(&self, path: &Path) -> bool {
        match self.mode() {
            PermissionMode::Yolo => true,
            PermissionMode::ReadOnly | PermissionMode::Plan | PermissionMode::AutoEdit => {
                self.is_inside_project(path)
            }
        }
    }

    /// May the agent write *anywhere at all* in the current mode?
    ///
    /// The coarse counterpart to [`Self::allows_write`], used by
    /// [`crate::ToolRegistry`] to gate a tool that declares
    /// [`crate::Capability::WRITE`] before it runs. `allows_write` still
    /// decides *which* paths; this decides whether writing is on the table.
    pub fn allows_any_write(&self) -> bool {
        match self.mode() {
            PermissionMode::AutoEdit | PermissionMode::Yolo => true,
            PermissionMode::Plan => self.plan_approved(),
            PermissionMode::ReadOnly => false,
        }
    }

    /// Permission for any shell execution.
    pub fn allows_shell(&self) -> bool {
        match self.mode() {
            PermissionMode::AutoEdit | PermissionMode::Yolo => true,
            PermissionMode::Plan => self.plan_approved(),
            PermissionMode::ReadOnly => false,
        }
    }

    /// Permission for outbound network access (web_fetch / web_search).
    ///
    /// Network egress is a data-exfiltration channel: content the agent reads
    /// (or prompt-injected instructions inside it) could smuggle secrets out
    /// via a URL or query string. So the read-only research modes can't reach
    /// the network — only `auto-edit`/`yolo`, where the user has already
    /// granted the agent latitude to act. The `[tools].allow_network` opt-in
    /// lifts this for users who want look-ups in read-only/plan too.
    pub fn allows_network(&self) -> bool {
        self.allow_network || matches!(self.mode(), PermissionMode::AutoEdit | PermissionMode::Yolo)
    }
}

/// Resolve `path` to an absolute form suitable for a project-containment
/// check, with `..`/`.` components folded away.
///
/// `std::fs::canonicalize` only works for paths that already exist; the
/// common write case — creating a *new* file — would otherwise fall back to
/// the raw, un-normalised path. A raw path like `<root>/../../etc/evil`
/// still has `<root>` as its leading components, so a naive
/// `starts_with(root)` check would wrongly judge it "inside the project" and
/// let an `auto-edit`-mode write escape the tree. To prevent that we
/// canonicalize the longest existing ancestor (resolving symlinks so a
/// symlinked parent can't smuggle the target out of the tree) and then
/// lexically fold the remaining, not-yet-existing components.
fn resolve_for_containment(path: &Path) -> PathBuf {
    // Peel off trailing components until we reach an ancestor that exists.
    //
    // `symlink_metadata`, not `exists()`: `exists()` follows symlinks, so a
    // *dangling* symlink reports false, gets peeled off as a "doesn't exist
    // yet" component, and is then folded back in lexically — landing inside
    // the project by construction. `write_file` would then follow it out of
    // the tree. A repo can commit such a link (`docs/notes.md ->
    // ../../../.ssh/authorized_keys`), so this must not depend on the target
    // existing. `symlink_metadata` succeeds for a dangling link, which stops
    // the peel and lets the containment check below see it for what it is.
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            // No parent (root) or no file name (e.g. trailing `..`): stop and
            // let lexical normalisation handle whatever is left.
            _ => break,
        }
    }
    let mut base = resolve_existing(existing, 0);
    for component in tail.iter().rev() {
        base.push(component);
    }
    normalize_lexical(&base)
}

/// Maximum symlink hops before we give up. Matches the usual OS limit closely
/// enough and bounds a link cycle.
const MAX_SYMLINK_HOPS: u32 = 16;

/// Resolve an existing path to its real location.
///
/// `canonicalize` handles the normal case, but it *fails* on a dangling
/// symlink — and falling back to the raw path there would put an escaping link
/// back inside the project. So dangling links are followed manually: read the
/// target, resolve it against the link's parent, and recurse.
fn resolve_existing(path: &Path, hops: u32) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    if hops >= MAX_SYMLINK_HOPS {
        return path.to_path_buf();
    }
    // Dangling symlink: follow it by hand so the escape is visible to the
    // containment check.
    if let Ok(md) = path.symlink_metadata() {
        if md.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(path) {
                let joined = if target.is_absolute() {
                    target
                } else {
                    path.parent().unwrap_or(Path::new("")).join(target)
                };
                let folded = normalize_lexical(&joined);
                return resolve_existing(&folded, hops + 1);
            }
        }
    }
    path.to_path_buf()
}

/// Lexically normalise a path: drop `.` components and resolve `..` by
/// popping the previous normal component. Does not touch the filesystem, so
/// it is safe for paths that don't exist. A leading `..` that would escape
/// the root is preserved (it cannot be popped), which keeps such a path from
/// matching any in-tree prefix.
fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop if the last pushed component is a normal segment;
                // never pop past a root/prefix.
                let popped = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if popped {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Split a shell command line into sub-commands (by unquoted `;`/`&`/`|`/
/// newline) and tokenize each into argv, honoring single/double quotes and
/// backslash escapes. Redirection operators (`<`, `>`) act as word
/// boundaries but don't start a new sub-command. Best-effort: this is a
/// safety filter, not a POSIX-complete parser.
fn tokenize_shell(s: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = s.chars().collect();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut cmd: Vec<String> = Vec::new();
    let mut tok = String::new();
    let mut in_tok = false;
    let mut quote: Option<char> = None;
    let mut i = 0;

    macro_rules! end_tok {
        () => {
            if in_tok {
                cmd.push(std::mem::take(&mut tok));
                in_tok = false;
            }
        };
    }
    macro_rules! end_cmd {
        () => {
            end_tok!();
            if !cmd.is_empty() {
                commands.push(std::mem::take(&mut cmd));
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                tok.push(c);
            }
            in_tok = true;
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_tok = true;
                i += 1;
            }
            '\\' => {
                if i + 1 < chars.len() {
                    tok.push(chars[i + 1]);
                    in_tok = true;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            ' ' | '\t' | '\r' => {
                end_tok!();
                i += 1;
            }
            ';' | '\n' => {
                end_cmd!();
                i += 1;
            }
            '&' | '|' => {
                end_cmd!();
                // Consume a doubled operator (`&&` / `||`).
                if i + 1 < chars.len() && chars[i + 1] == c {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '<' | '>' => {
                end_tok!();
                i += 1;
            }
            _ => {
                tok.push(c);
                in_tok = true;
                i += 1;
            }
        }
    }
    if in_tok {
        cmd.push(tok);
    }
    if !cmd.is_empty() {
        commands.push(cmd);
    }
    commands
}

/// True if a single tokenized sub-command matches a tokenized denylist
/// pattern. See [`ToolCtx::is_shell_denied`] for the matching rules.
fn command_matches_pattern(cmd: &[String], pat: &[String]) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if pat.len() == 1 {
        let p = pat[0].as_str();
        // Program match (by basename) — blocks `sudo`, `/usr/bin/sudo`, etc.
        if let Some(prog) = cmd.first() {
            if program_basename(prog).eq_ignore_ascii_case(p) {
                return true;
            }
        }
        // Or an exact standalone argument token.
        return cmd.iter().any(|t| t == p);
    }
    if pat.len() > cmd.len() {
        return false;
    }
    cmd.windows(pat.len()).any(|w| w == pat)
}

/// Basename of a program token: the part after the last `/` or `\`.
fn program_basename(prog: &str) -> &str {
    prog.rsplit(['/', '\\']).next().unwrap_or(prog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "wingman-ctx-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn write_inside_project_is_allowed() {
        let root = unique_tmp_dir();
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        // A brand-new (non-existent) file under the project root must pass.
        let target = ctx.resolve("src/new_file.rs");
        assert!(ctx.is_inside_project(&target));
        assert!(ctx.allows_write(&target));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repo can commit a symlink pointing outside the tree. If its target
    /// doesn't exist yet, `exists()` reports false and the old peel logic
    /// folded the link name back in-tree — so `write_file` followed it out and
    /// created a file at the target (`~/.ssh/authorized_keys`,
    /// `~/.bashrc`, …). The link must be resolved, not treated as a plain
    /// not-yet-created file.
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_out_of_tree_is_denied() {
        let base = std::env::temp_dir().join(format!("wm-symlink-{}", std::process::id()));
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();

        // Target deliberately does NOT exist -> dangling.
        let link = root.join("docs").join("notes.md");
        std::os::unix::fs::symlink(base.join("outside").join("pwned.txt"), &link).unwrap();

        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        assert!(
            !ctx.allows_write(&link),
            "write through a dangling symlink must not be judged inside the project"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `plan` used to be byte-identical to `read-only`: `present_plan` gated
    /// nothing and there was no promotion step, while the README sold it as
    /// "the agent must call present_plan before any edit".
    #[test]
    fn plan_mode_denies_until_the_plan_is_approved() {
        let root = std::env::temp_dir().join(format!("wm-plan-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("src.rs");

        let ctx = ToolCtx::new(PermissionMode::Plan, root.clone(), root.clone());

        // Before approval: identical to read-only.
        assert!(!ctx.allows_write(&target));
        assert!(!ctx.allows_any_write());
        assert!(!ctx.allows_shell());
        assert!(!ctx.plan_approved());

        // Reads were always fine, and must stay fine.
        assert!(ctx.allows_read(&target));

        ctx.approve_plan();

        // After approval: behaves like auto-edit, still project-confined.
        assert!(ctx.plan_approved());
        assert!(ctx.allows_write(&target));
        assert!(ctx.allows_any_write());
        assert!(ctx.allows_shell());
        assert!(
            !ctx.allows_write(std::path::Path::new("/etc/passwd")),
            "approval must not lift project containment"
        );
        assert!(
            !ctx.allows_write(&root.join(".git").join("hooks").join("pre-commit")),
            "approval must not lift protected paths"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Approval is consent for *this* plan. Leaving plan mode drops it, so
    /// coming back later doesn't silently inherit permission for other work.
    #[test]
    fn leaving_plan_mode_revokes_approval() {
        let root = std::env::temp_dir().join(format!("wm-planrevoke-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let ctx = ToolCtx::new(PermissionMode::Plan, root.clone(), root.clone());
        ctx.approve_plan();
        assert!(ctx.plan_approved());

        ctx.set_mode(PermissionMode::ReadOnly);
        ctx.set_mode(PermissionMode::Plan);

        assert!(
            !ctx.plan_approved(),
            "returning to plan mode must require a fresh approval"
        );
        assert!(!ctx.allows_shell());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// One injected edit must not be able to buy persistence. These paths stay
    /// refused even in `yolo`, which is the whole point of the list.
    #[test]
    fn protected_paths_are_refused_in_every_mode() {
        let root = std::env::temp_dir().join(format!("wm-protected-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git").join("hooks")).unwrap();
        std::fs::create_dir_all(root.join(".wingman").join("skills")).unwrap();

        let protected = [
            root.join(".git").join("hooks").join("pre-commit"),
            root.join(".git").join("config"),
            root.join(".wingman").join("config.toml"),
            root.join(".wingman").join("skills").join("evil.md"),
            root.join(".wingman").join("trusted.toml"),
        ];

        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::Plan,
            PermissionMode::AutoEdit,
            PermissionMode::Yolo,
        ] {
            let ctx = ToolCtx::new(mode, root.clone(), root.clone());
            for p in &protected {
                assert!(
                    !ctx.allows_write(p),
                    "{} must be refused in {mode:?}",
                    p.display()
                );
            }
        }

        // Ordinary project files under .wingman are still writable.
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        assert!(ctx.allows_write(&root.join(".wingman").join("memory").join("note.md")));
        assert!(ctx.allows_write(&root.join("src.rs")));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The in-tree case must keep working — this is an ordinary new file.
    #[test]
    fn new_file_inside_project_is_allowed() {
        let root = std::env::temp_dir().join(format!("wm-newfile-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        assert!(ctx.allows_write(&root.join("src").join("brand_new.rs")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parent_traversal_to_nonexistent_path_is_denied() {
        let root = unique_tmp_dir();
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        // Regression: a `..` escape to a not-yet-existing file used to slip
        // through because canonicalize() fails on missing paths and the raw
        // path still textually starts with the project root.
        let escape = ctx.resolve("../../../../tmp/wingman-escape-evil");
        assert!(
            !ctx.is_inside_project(&escape),
            "`..` traversal must not be judged inside the project"
        );
        assert!(!ctx.allows_write(&escape));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parent_traversal_to_existing_path_is_denied() {
        let root = unique_tmp_dir();
        let outside = root
            .parent()
            .unwrap()
            .join(format!("wingman-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, b"secret").unwrap();
        let ctx = ToolCtx::new(PermissionMode::AutoEdit, root.clone(), root.clone());
        let rel = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        let target = ctx.resolve(&rel);
        assert!(!ctx.is_inside_project(&target));
        assert!(!ctx.allows_write(&target));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_lexical_folds_dot_and_parent() {
        assert_eq!(
            normalize_lexical(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    fn ctx_with_denylist(patterns: &[&str]) -> ToolCtx {
        ToolCtx::new_with_config(
            PermissionMode::Yolo,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            patterns.iter().map(|s| s.to_string()).collect(),
            false,
        )
    }

    #[test]
    fn set_mode_re_gates_live_and_is_shared_across_clones() {
        // Use a real existing dir as the project root: containment canonicalizes
        // both the root and the resolved path, and on Windows a non-existent
        // root like "/tmp" fails to canonicalize (staying drive-less) while the
        // resolved path gets drive-qualified, so `starts_with` spuriously fails.
        let root = std::env::temp_dir();
        let child = root.join("wingman-ctx-test-x");
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, root.clone(), root.clone());
        assert_eq!(ctx.mode(), PermissionMode::ReadOnly);
        assert!(!ctx.allows_shell());
        assert!(!ctx.allows_write(&child));

        // A clone (as the running agent's registry holds) sees the switch.
        let shared = ctx.clone();
        ctx.set_mode(PermissionMode::AutoEdit);
        assert_eq!(shared.mode(), PermissionMode::AutoEdit);
        assert!(shared.allows_shell());
        assert!(shared.allows_write(&child));
    }

    #[test]
    fn denylist_matches_exact_command() {
        let ctx = ctx_with_denylist(&["rm -rf /"]);
        assert!(ctx.is_shell_denied("rm -rf /"));
    }

    #[test]
    fn denylist_resists_whitespace_bypass() {
        let ctx = ctx_with_denylist(&["rm -rf /"]);
        // Extra spaces/tabs used to slip past the substring check.
        assert!(ctx.is_shell_denied("rm   -rf\t/"));
    }

    #[test]
    fn denylist_resists_operator_chaining_bypass() {
        let ctx = ctx_with_denylist(&["sudo"]);
        // Hiding a denied program behind `&&`/`;`/`|` used to bypass.
        assert!(ctx.is_shell_denied("true && sudo rm -rf /"));
        assert!(ctx.is_shell_denied("echo hi; sudo -k"));
        assert!(ctx.is_shell_denied("cat x | sudo tee y"));
    }

    #[test]
    fn denylist_matches_program_by_basename() {
        let ctx = ctx_with_denylist(&["sudo"]);
        assert!(ctx.is_shell_denied("/usr/bin/sudo reboot"));
    }

    #[test]
    fn denylist_no_false_positive_on_substring() {
        let ctx = ctx_with_denylist(&["sudo"]);
        // `pseudo` contains "sudo" but must not be denied.
        assert!(!ctx.is_shell_denied("pseudo-terminal --help"));
    }

    #[test]
    fn denylist_quoted_operator_is_not_a_separator() {
        let ctx = ctx_with_denylist(&["sudo"]);
        // The `;` is inside quotes — it's an argument, not a command break,
        // and there is no actual `sudo` program invocation here.
        assert!(!ctx.is_shell_denied("echo 'run sudo later'"));
    }

    #[test]
    fn denylist_empty_denies_nothing() {
        let ctx = ctx_with_denylist(&[]);
        assert!(!ctx.is_shell_denied("rm -rf /"));
    }

    #[test]
    fn network_gated_to_edit_modes() {
        let root = std::env::temp_dir();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, root.clone(), root.clone());
        assert!(
            !ctx.allows_network(),
            "read-only must not reach the network"
        );
        ctx.set_mode(PermissionMode::Plan);
        assert!(!ctx.allows_network(), "plan must not reach the network");
        ctx.set_mode(PermissionMode::AutoEdit);
        assert!(ctx.allows_network(), "auto-edit may reach the network");
        ctx.set_mode(PermissionMode::Yolo);
        assert!(ctx.allows_network(), "yolo may reach the network");
    }

    #[test]
    fn allow_network_opt_in_lifts_read_only_gate() {
        let root = std::env::temp_dir();
        let ctx = ToolCtx::new_with_config(
            PermissionMode::ReadOnly,
            root.clone(),
            root.clone(),
            Vec::new(),
            true,
        );
        assert!(
            ctx.allows_network(),
            "allow_network opt-in permits network even in read-only"
        );
    }
}
