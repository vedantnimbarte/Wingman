//! J12 — skill packs (shareable, versioned agent definitions).
//!
//! Role definitions in `~/.wingman/agents/<role>.md` become shareable,
//! semver-pinned bundles: a directory of role markdown + lessons + tool
//! registrations + acceptance templates, installed from a git repo or
//! local path. Configured via `[pilot.skills].packs` as
//! `owner/name@semver` strings.
//!
//! Packs are found through a registry index: a git repo (or local directory)
//! holding [`INDEX_FILE`], which maps `owner/name` to published versions, each
//! with a source URL, dependency specs and an SSH signature. [`resolve`] picks
//! the newest version satisfying every caret requirement, transitively;
//! [`fetch_pack`] refuses unsigned packs unless told otherwise, and checks a
//! signature with `ssh-keygen -Y verify` against
//! `~/.wingman/packs/allowed_signers`, the owner as the principal. The signed
//! message is [`signed_payload`]: the exact pack, a digest of its files, and
//! its dependencies, so neither the source nor the index can swap either.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The index file at the root of an index repo or directory.
pub const INDEX_FILE: &str = "index.json";

/// SSH signature namespace (`ssh-keygen -n`), so a signature made for git
/// commits or anything else cannot be replayed as a pack signature.
pub const SIGNATURE_NAMESPACE: &str = "wingman-skillpack";

/// A reference to a pack: `owner/name@version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRef {
    pub owner: String,
    pub name: String,
    pub version: SemVer,
}

impl PackRef {
    /// Slug used as the on-disk directory name: `owner__name@version`.
    pub fn slug(&self) -> String {
        format!("{}__{}@{}", self.owner, self.name, self.version)
    }

    /// Install path under `<home>/.wingman/packs/<slug>/`.
    pub fn install_path(&self, home: &Path) -> PathBuf {
        packs_dir(home).join(self.slug())
    }

    /// `owner/name`, the index key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl std::fmt::Display for PackRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}@{}", self.owner, self.name, self.version)
    }
}

/// `<home>/.wingman/packs/`: installed packs, their receipts, the index cache
/// and the `allowed_signers` trust file.
pub fn packs_dir(home: &Path) -> PathBuf {
    home.join(".wingman").join("packs")
}

/// A minimal semantic version (`major.minor` or `major.minor.patch`).
/// Ordering is field order: major, then minor, then patch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl SemVer {
    /// Is `self` compatible with `required` under caret rules (same major,
    /// `self >= required`)? Used to decide whether an installed pack
    /// satisfies a spec.
    pub fn satisfies(&self, required: &SemVer) -> bool {
        self.major == required.major && (self.minor, self.patch) >= (required.minor, required.patch)
    }
}

/// Parse a semver of the form `X.Y` or `X.Y.Z`. Missing patch defaults to 0.
pub fn parse_semver(s: &str) -> Result<SemVer, String> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(format!("bad semver `{s}` (expect X.Y or X.Y.Z)"));
    }
    let num = |p: &str| {
        p.parse::<u32>()
            .map_err(|_| format!("bad semver component `{p}`"))
    };
    Ok(SemVer {
        major: num(parts[0])?,
        minor: num(parts[1])?,
        patch: if parts.len() == 3 { num(parts[2])? } else { 0 },
    })
}

/// Parse an `owner/name@version` pack spec.
pub fn parse_pack_ref(spec: &str) -> Result<PackRef, String> {
    let spec = spec.trim();
    let (path, version) = spec
        .rsplit_once('@')
        .ok_or_else(|| format!("pack spec `{spec}` missing `@version`"))?;
    let (owner, name) = path
        .split_once('/')
        .ok_or_else(|| format!("pack spec `{spec}` missing `owner/`"))?;
    if owner.is_empty() || name.is_empty() {
        return Err(format!("pack spec `{spec}` has empty owner or name"));
    }
    if !valid_ident(owner) || !valid_ident(name) {
        return Err(format!(
            "pack spec `{spec}` has invalid owner/name characters"
        ));
    }
    Ok(PackRef {
        owner: owner.to_string(),
        name: name.to_string(),
        version: parse_semver(version)?,
    })
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// What a pack directory contains, once installed. Paths are relative to
/// the pack root.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PackManifest {
    /// `<role>.md` role-definition files.
    pub roles: Vec<String>,
    /// `<role>.lessons.md` files.
    pub lessons: Vec<String>,
    /// Tool registration files under `tools/`.
    pub tools: Vec<String>,
    /// Acceptance-template files.
    pub acceptance_templates: Vec<String>,
}

/// Parse a batch of `[pilot.skills].packs` specs, returning the parsed
/// refs and any per-spec errors (so one bad entry doesn't sink the rest).
pub fn parse_pack_list(specs: &[String]) -> (Vec<PackRef>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errs = Vec::new();
    for s in specs {
        match parse_pack_ref(s) {
            Ok(r) => ok.push(r),
            Err(e) => errs.push(e),
        }
    }
    (ok, errs)
}

/// Scan an installed pack directory into a [`PackManifest`]: `<role>.md`
/// role files (excluding `.lessons.md`), `<role>.lessons.md` lessons,
/// anything under `tools/`, and `*.acceptance.json` templates. Paths are
/// relative to `pack_dir`.
pub fn scan_manifest(pack_dir: &Path) -> std::io::Result<PackManifest> {
    let mut m = PackManifest::default();
    let Ok(entries) = std::fs::read_dir(pack_dir) else {
        return Ok(m);
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        if is_file && name.ends_with(".lessons.md") {
            m.lessons.push(name);
        } else if is_file && name.ends_with(".acceptance.json") {
            m.acceptance_templates.push(name);
        } else if is_file && name.ends_with(".md") {
            m.roles.push(name);
        } else if name == "tools" && e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Ok(tools) = std::fs::read_dir(e.path()) {
                for t in tools.flatten() {
                    m.tools
                        .push(format!("tools/{}", t.file_name().to_string_lossy()));
                }
            }
        }
    }
    m.roles.sort();
    m.lessons.sort();
    m.tools.sort();
    m.acceptance_templates.sort();
    Ok(m)
}

/// Install a fetched pack's role + lessons files into `<home>/.wingman/agents/`
/// so the existing [`crate::role`] loader picks them up with no code change.
/// Returns the agent-file names written. Tool registrations and acceptance
/// templates stay in the pack dir (loaded lazily by their own consumers).
pub fn install_pack_files(pack_dir: &Path, home: &Path) -> std::io::Result<Vec<String>> {
    let manifest = scan_manifest(pack_dir)?;
    let agents = home.join(".wingman").join("agents");
    std::fs::create_dir_all(&agents)?;
    let mut written = Vec::new();
    for rel in manifest.roles.iter().chain(manifest.lessons.iter()) {
        std::fs::copy(pack_dir.join(rel), agents.join(rel))?;
        written.push(rel.clone());
    }
    Ok(written)
}

/// One published version of a pack, as listed in [`INDEX_FILE`].
#[derive(Debug, Clone, Deserialize)]
pub struct IndexEntry {
    pub version: String,
    /// Git URL (cloned at tag `v<version>`) or, for a local index, a directory.
    pub source: String,
    /// `ssh-keygen -Y sign` output over [`signed_payload`]. Absent = unsigned.
    #[serde(default)]
    pub signature: Option<String>,
    /// `owner/name@X.Y[.Z]` caret requirements.
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub description: String,
}

/// A parsed [`INDEX_FILE`]: `{"packs": {"owner/name": [IndexEntry, ...]}}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PackIndex {
    #[serde(default)]
    pub packs: BTreeMap<String, Vec<IndexEntry>>,
    /// Loaded from a git remote. A remote index may only point at remote
    /// sources: a local path in it would have the installer copy an arbitrary
    /// directory of this machine into the agents dir.
    #[serde(skip)]
    remote: bool,
}

/// Load the index at `location`: a local directory containing [`INDEX_FILE`],
/// or a git URL shallow-cloned afresh into `<packs>/.index/`.
pub fn load_index(
    runner: &dyn crate::pr::CommandRunner,
    location: &str,
    home: &Path,
) -> Result<PackIndex, String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("no skill-pack index configured (set [pilot.skills].index)".into());
    }
    let (dir, remote) = if Path::new(location).is_dir() {
        (PathBuf::from(location), false)
    } else {
        if !is_safe_clone_url(location) {
            return Err(format!(
                "refusing to clone skill-pack index from unsafe source '{location}' \
                 (only https:// and git@host: are allowed)"
            ));
        }
        let cache = packs_dir(home).join(".index");
        if cache.exists() {
            std::fs::remove_dir_all(&cache)
                .map_err(|e| format!("cannot clear index cache {}: {e}", cache.display()))?;
        }
        std::fs::create_dir_all(packs_dir(home)).map_err(|e| format!("mkdir failed: {e}"))?;
        let cache_str = cache.to_string_lossy().to_string();
        let out = runner
            .run(
                "git",
                &["clone", "--depth", "1", "--", location, &cache_str],
                home,
            )
            .map_err(|e| format!("git clone spawn failed: {e}"))?;
        if !out.success() {
            return Err(format!("index clone failed: {}", out.stderr.trim()));
        }
        (cache, true)
    };
    let path = dir.join(INDEX_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut index: PackIndex =
        serde_json::from_str(&text).map_err(|e| format!("bad {}: {e}", path.display()))?;
    index.remote = remote;
    Ok(index)
}

/// Packs whose `owner/name` or description contains `query` (case-insensitive;
/// empty matches all), newest version of each.
pub fn search<'a>(index: &'a PackIndex, query: &str) -> Vec<(&'a str, &'a IndexEntry)> {
    let q = query.to_lowercase();
    index
        .packs
        .iter()
        .filter_map(|(key, entries)| {
            let newest = entries
                .iter()
                .filter_map(|e| parse_semver(&e.version).ok().map(|v| (v, e)))
                .max_by(|a, b| a.0.cmp(&b.0))?
                .1;
            let hit =
                key.to_lowercase().contains(&q) || newest.description.to_lowercase().contains(&q);
            hit.then_some((key.as_str(), newest))
        })
        .collect()
}

/// A pack chosen by [`resolve`], ready for [`fetch_pack`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPack {
    pub pack: PackRef,
    pub source: String,
    pub signature: Option<String>,
    pub deps: Vec<PackRef>,
}

/// Resolve `roots` and their dependencies against `index`: each `owner/name`
/// gets the newest indexed version satisfying every caret requirement on it,
/// or an error naming the requirements that conflict. One version per pack,
/// since packs install their roles into one shared agents dir.
///
/// ponytail: requirements only accumulate and nothing backtracks, so a dep of a
/// version later superseded still constrains; that can report a conflict a
/// backtracking resolver would solve. Add backtracking when an index is big
/// enough for that to bite.
pub fn resolve(index: &PackIndex, roots: &[PackRef]) -> Result<Vec<ResolvedPack>, String> {
    // owner/name -> [(minimum version, who asked)]
    let mut reqs: BTreeMap<String, Vec<(SemVer, String)>> = BTreeMap::new();
    for r in roots {
        reqs.entry(r.key())
            .or_default()
            .push((r.version.clone(), "requested".into()));
    }
    loop {
        let mut chosen: BTreeMap<String, (SemVer, &IndexEntry)> = BTreeMap::new();
        for (key, wants) in &reqs {
            let entries = index
                .packs
                .get(key)
                .ok_or_else(|| format!("pack `{key}` is not in the index"))?;
            let versions: Vec<(SemVer, &IndexEntry)> = entries
                .iter()
                .filter_map(|e| parse_semver(&e.version).ok().map(|v| (v, e)))
                .collect();
            let best = versions
                .iter()
                .filter(|(v, _)| wants.iter().all(|(w, _)| v.satisfies(w)))
                .max_by(|a, b| a.0.cmp(&b.0));
            let Some((v, e)) = best else {
                let asked: Vec<String> =
                    wants.iter().map(|(w, by)| format!("^{w} ({by})")).collect();
                let have: Vec<String> = versions.iter().map(|(v, _)| v.to_string()).collect();
                return Err(format!(
                    "version conflict for `{key}`: nothing satisfies {}; index has [{}]",
                    asked.join(", "),
                    have.join(", ")
                ));
            };
            if index.remote && !is_safe_clone_url(&e.source) {
                return Err(format!(
                    "index entry {key}@{v} has non-git source '{}'",
                    e.source
                ));
            }
            chosen.insert(key.clone(), (v.clone(), *e));
        }
        let mut grew = false;
        for (key, (v, e)) in &chosen {
            let by = format!("{key}@{v}");
            for d in &e.deps {
                let dep = parse_pack_ref(d).map_err(|err| format!("{by}: {err}"))?;
                let wants = reqs.entry(dep.key()).or_default();
                if !wants.iter().any(|(w, b)| *w == dep.version && *b == by) {
                    wants.push((dep.version, by.clone()));
                    grew = true;
                }
            }
        }
        if !grew {
            return chosen
                .into_iter()
                .map(|(key, (version, e))| {
                    let (owner, name) = key.split_once('/').unwrap_or_default();
                    Ok(ResolvedPack {
                        pack: PackRef {
                            owner: owner.to_string(),
                            name: name.to_string(),
                            version,
                        },
                        source: e.source.clone(),
                        signature: e.signature.clone(),
                        deps: e
                            .deps
                            .iter()
                            .map(|d| parse_pack_ref(d))
                            .collect::<Result<_, _>>()?,
                    })
                })
                .collect();
        }
    }
}

/// `sha256:<hex>` over every file under `dir` (skipping `.git`), in sorted
/// `/`-separated relative-path order, each framed as path, NUL, length,
/// bytes. Symlinks are refused: what they point at is not part of the pack.
pub fn pack_digest(dir: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for e in entries {
            let e = e.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
            if e.file_name() == ".git" {
                continue;
            }
            let path = e.path();
            let kind = e
                .file_type()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if kind.is_symlink() {
                return Err(format!(
                    "packs may not contain symlinks: {}",
                    path.display()
                ));
            } else if kind.is_dir() {
                walk(root, &path, out)?;
            } else {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for rel in &files {
        let bytes = std::fs::read(dir.join(rel)).map_err(|e| format!("cannot read {rel}: {e}"))?;
        h.update(rel.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(format!("sha256:{}", crate::httpsig::to_hex(&h.finalize())))
}

/// The exact bytes a pack author signs (and [`verify_signature`] checks).
/// Dependencies are normalised to `X.Y.Z` and sorted, so `1.0` and `1.0.0`
/// sign the same.
pub fn signed_payload(pack: &PackRef, digest: &str, deps: &[PackRef]) -> String {
    let mut deps: Vec<String> = deps.iter().map(|d| d.to_string()).collect();
    deps.sort();
    deps.insert(0, "deps".into());
    format!(
        "wingman-skillpack v1\npack {pack}\ndigest {digest}\n{}\n",
        deps.join(" ")
    )
}

/// Check `signature` over `payload` with `ssh-keygen -Y verify`, against
/// `<packs>/allowed_signers` with the pack owner as the principal: a key
/// trusted for `acme` cannot vouch for `evil/...`. Fails closed when the trust
/// file or `ssh-keygen` is missing.
pub fn verify_signature(
    runner: &dyn crate::pr::CommandRunner,
    pack: &PackRef,
    payload: &str,
    signature: &str,
    home: &Path,
) -> Result<(), String> {
    let dir = packs_dir(home);
    let signers = dir.join("allowed_signers");
    if !signers.is_file() {
        return Err(format!(
            "cannot verify {pack}: no trusted signers at {} (one line per key: \
             `<owner> namespaces=\"{SIGNATURE_NAMESPACE}\" <ssh public key>`)",
            signers.display()
        ));
    }
    let sig = dir.join(format!("{}.sig", pack.slug()));
    std::fs::write(&sig, signature).map_err(|e| format!("cannot write {}: {e}", sig.display()))?;
    let signers_str = signers.to_string_lossy().to_string();
    let sig_str = sig.to_string_lossy().to_string();
    let out = runner
        .run_with_stdin(
            "ssh-keygen",
            &[
                "-Y",
                "verify",
                "-f",
                &signers_str,
                "-I",
                &pack.owner,
                "-n",
                SIGNATURE_NAMESPACE,
                "-s",
                &sig_str,
            ],
            &dir,
            payload.as_bytes(),
        )
        .map_err(|e| format!("verifying {pack} needs ssh-keygen (OpenSSH 8.1+) on PATH: {e}"))?;
    if !out.success() {
        let why = format!("{} {}", out.stderr.trim(), out.stdout.trim());
        return Err(format!("bad signature for {pack}: {}", why.trim()));
    }
    Ok(())
}

/// What was installed, written beside the pack as `<packs>/<slug>.json` so
/// `list` and `verify` work without the index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// `owner/name@X.Y.Z`.
    pub pack: String,
    pub source: String,
    /// [`pack_digest`] at install time.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
}

/// Fetch a pack into its versioned install path, verify it, and install its
/// agent files. A git `source` is `git clone --depth 1 --branch v<version>`-ed
/// via the [`CommandRunner`] seam; a local `source` (existing directory) is
/// copied. An install path that already exists is re-verified rather than
/// re-fetched (the slug pins the exact version).
///
/// An unsigned pack is refused before anything is fetched unless
/// `allow_unsigned`; a signed one must verify whatever the flag says. Content
/// that fails verification is removed, so a later run cannot mistake it for
/// an installed pack.
///
/// [`CommandRunner`]: crate::pr::CommandRunner
pub fn fetch_pack(
    runner: &dyn crate::pr::CommandRunner,
    resolved: &ResolvedPack,
    home: &Path,
    allow_unsigned: bool,
) -> Result<PathBuf, String> {
    let pack = &resolved.pack;
    let source = resolved.source.as_str();
    if resolved.signature.is_none() && !allow_unsigned {
        return Err(format!(
            "refusing unsigned pack {pack} (pass --allow-unsigned to install it anyway)"
        ));
    }
    let dest = pack.install_path(home);
    if !dest.exists() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
        }
        let local = Path::new(source);
        if local.is_dir() {
            copy_dir_recursive(local, &dest).map_err(|e| format!("local copy failed: {e}"))?;
        } else {
            // Only clone over https/ssh. Git's `ext::`/`file://` transports are
            // an RCE vector (`git clone 'ext::sh -c evil'`); reject anything
            // that isn't a normal remote URL so a hostile pack manifest can't
            // run commands during install.
            if !is_safe_clone_url(source) {
                return Err(format!(
                    "refusing to clone skillpack from unsafe source '{source}' \
                     (only https:// and git@host: are allowed)"
                ));
            }
            let tag = format!("v{}", pack.version);
            let dest_str = dest.to_string_lossy().to_string();
            let out = runner
                .run(
                    "git",
                    // autocrlf off: a CRLF checkout on Windows would change
                    // the digest the author signed. `--` terminates options so
                    // a source starting with `-` can't be parsed as a flag.
                    &[
                        "-c",
                        "core.autocrlf=false",
                        "clone",
                        "--depth",
                        "1",
                        "--branch",
                        &tag,
                        "--",
                        source,
                        &dest_str,
                    ],
                    home,
                )
                .map_err(|e| format!("git clone spawn failed: {e}"))?;
            if !out.success() {
                return Err(format!("git clone failed: {}", out.stderr.trim()));
            }
        }
    }
    let checked = pack_digest(&dest).and_then(|digest| {
        if let Some(sig) = &resolved.signature {
            let payload = signed_payload(pack, &digest, &resolved.deps);
            verify_signature(runner, pack, &payload, sig, home)?;
        }
        Ok(digest)
    });
    let digest = match checked {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(format!("{e} (removed {})", dest.display()));
        }
    };
    install_pack_files(&dest, home).map_err(|e| format!("install failed: {e}"))?;
    let receipt = Receipt {
        pack: pack.to_string(),
        source: source.to_string(),
        digest,
        signature: resolved.signature.clone(),
        deps: resolved.deps.iter().map(|d| d.to_string()).collect(),
    };
    let path = packs_dir(home).join(format!("{}.json", pack.slug()));
    let json = serde_json::to_string_pretty(&receipt).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(dest)
}

/// Every install receipt under `<packs>/`, sorted by pack, plus one error per
/// unreadable receipt.
pub fn list_installed(home: &Path) -> (Vec<Receipt>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errs = Vec::new();
    let Ok(entries) = std::fs::read_dir(packs_dir(home)) else {
        return (ok, errs);
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") || !path.is_file() {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Receipt>(&t).map_err(|e| e.to_string()))
        {
            Ok(r) => ok.push(r),
            Err(err) => errs.push(format!("{}: {err}", path.display())),
        }
    }
    ok.sort_by(|a, b| a.pack.cmp(&b.pack));
    (ok, errs)
}

/// Re-check an installed pack: a signed one's current files must still verify
/// against its signature; an unsigned one must match its install-time digest,
/// and is itself a failure unless `allow_unsigned`.
pub fn verify_installed(
    runner: &dyn crate::pr::CommandRunner,
    receipt: &Receipt,
    home: &Path,
    allow_unsigned: bool,
) -> Result<(), String> {
    let pack = parse_pack_ref(&receipt.pack)?;
    let dest = pack.install_path(home);
    if !dest.is_dir() {
        return Err(format!("{pack} is not installed at {}", dest.display()));
    }
    let digest = pack_digest(&dest)?;
    match &receipt.signature {
        Some(sig) => {
            let deps: Vec<PackRef> = receipt
                .deps
                .iter()
                .map(|d| parse_pack_ref(d))
                .collect::<Result<_, _>>()?;
            let payload = signed_payload(&pack, &digest, &deps);
            verify_signature(runner, &pack, &payload, sig, home)
        }
        None if digest != receipt.digest => {
            Err(format!("{pack} changed on disk since it was installed"))
        }
        None if !allow_unsigned => Err(format!("{pack} is unsigned")),
        None => Ok(()),
    }
}

/// Whether `source` is a git remote URL safe to `clone`. Allows https/http and
/// ssh (both `ssh://…` and scp-style `git@host:path`); rejects git's
/// command-executing transports (`ext::`, `fd::`, …) and `file://`.
fn is_safe_clone_url(source: &str) -> bool {
    let s = source.trim();
    if s.starts_with("https://") || s.starts_with("http://") || s.starts_with("ssh://") {
        return true;
    }
    // scp-style `user@host:path`: a single-colon remote with no `::` transport
    // marker. The `::` check is what rejects `ext::` / `fd::`.
    !s.contains("::") && s.contains('@') && s.contains(':')
}

/// Recursively copy a directory tree (skipping `.git`).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        let from = e.path();
        let to = dst.join(&name);
        if e.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_clone_url_rejects_command_transports() {
        assert!(is_safe_clone_url("https://github.com/u/repo.git"));
        assert!(is_safe_clone_url("ssh://git@github.com/u/repo.git"));
        assert!(is_safe_clone_url("git@github.com:u/repo.git"));
        // The dangerous ones: git's ext/fd transports and local file://.
        assert!(!is_safe_clone_url("ext::sh -c 'touch /tmp/pwned'"));
        assert!(!is_safe_clone_url("fd::17/foo"));
        assert!(!is_safe_clone_url("file:///etc/passwd"));
        assert!(!is_safe_clone_url("/local/path"));
    }

    #[test]
    fn j12_fetch_local_pack_installs_roles_into_agents() {
        // A local "pack" dir with a role + lessons + a tool + a stray file.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("tool-smith.md"), "# custom smith").unwrap();
        std::fs::write(src.path().join("developer.lessons.md"), "- lesson").unwrap();
        std::fs::create_dir(src.path().join("tools")).unwrap();
        std::fs::write(src.path().join("tools").join("query_db.json"), "{}").unwrap();
        std::fs::write(src.path().join("README"), "ignore me").unwrap();

        let m = scan_manifest(src.path()).unwrap();
        assert_eq!(m.roles, vec!["tool-smith.md"]);
        assert_eq!(m.lessons, vec!["developer.lessons.md"]);
        assert_eq!(m.tools, vec!["tools/query_db.json"]);

        // Install via the local-source path (no git); files land in agents/.
        let home = tempfile::tempdir().unwrap();
        let resolved = unsigned("acme/smithpack@1.0", &src.path().to_string_lossy());
        let runner = crate::pr::SystemCommandRunner;
        let dest = fetch_pack(&runner, &resolved, home.path(), true).unwrap();
        assert!(dest.ends_with("acme__smithpack@1.0.0"));
        let agents = home.path().join(".wingman").join("agents");
        assert!(agents.join("tool-smith.md").exists());
        assert!(agents.join("developer.lessons.md").exists());

        // The receipt makes it listable and re-verifiable without the index.
        let (receipts, errs) = list_installed(home.path());
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].pack, "acme/smithpack@1.0.0");
        assert_eq!(receipts[0].signature, None);
        verify_installed(&runner, &receipts[0], home.path(), true).unwrap();
        let err = verify_installed(&runner, &receipts[0], home.path(), false).unwrap_err();
        assert!(err.contains("unsigned"), "{err}");

        // Editing the installed pack is caught even though nothing signed it.
        std::fs::write(dest.join("tool-smith.md"), "# tampered").unwrap();
        let err = verify_installed(&runner, &receipts[0], home.path(), true).unwrap_err();
        assert!(err.contains("changed on disk"), "{err}");
    }

    fn unsigned(spec: &str, source: &str) -> ResolvedPack {
        ResolvedPack {
            pack: parse_pack_ref(spec).unwrap(),
            source: source.to_string(),
            signature: None,
            deps: Vec::new(),
        }
    }

    /// (program, args, stdin)
    type Call = (String, Vec<String>, Vec<u8>);
    type Reply = Box<dyn Fn(&str, &[&str]) -> crate::pr::CommandOut + Send + Sync>;

    /// Records every command and answers from `reply(program, args)`.
    struct FakeRunner {
        calls: std::sync::Mutex<Vec<Call>>,
        reply: Reply,
    }

    impl FakeRunner {
        fn new(
            reply: impl Fn(&str, &[&str]) -> crate::pr::CommandOut + Send + Sync + 'static,
        ) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                reply: Box::new(reply),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl crate::pr::CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            cwd: &Path,
        ) -> std::io::Result<crate::pr::CommandOut> {
            self.run_with_stdin(program, args, cwd, &[])
        }

        fn run_with_stdin(
            &self,
            program: &str,
            args: &[&str],
            _cwd: &Path,
            stdin: &[u8],
        ) -> std::io::Result<crate::pr::CommandOut> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
                stdin.to_vec(),
            ));
            Ok((self.reply)(program, args))
        }
    }

    fn exit(code: i32) -> crate::pr::CommandOut {
        crate::pr::CommandOut {
            status: Some(code),
            stdout: String::new(),
            stderr: if code == 0 { "" } else { "no good" }.into(),
        }
    }

    fn index(json: &str) -> PackIndex {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn unsigned_pack_is_refused_before_anything_is_fetched() {
        let home = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|_, _| exit(0));
        let resolved = unsigned("acme/x@1.0", "https://github.com/acme/x");
        let err = fetch_pack(&runner, &resolved, home.path(), false).unwrap_err();
        assert!(err.contains("--allow-unsigned"), "{err}");
        assert!(runner.calls().is_empty());
        assert!(!resolved.pack.install_path(home.path()).exists());
    }

    #[test]
    fn git_clone_pins_the_tag_and_keeps_line_endings() {
        let home = tempfile::tempdir().unwrap();
        // "Clone" by writing a role into the destination git was given.
        let runner = FakeRunner::new(|_, args| {
            let dest = Path::new(args.last().unwrap());
            std::fs::create_dir_all(dest).unwrap();
            std::fs::write(dest.join("r.md"), "# r").unwrap();
            exit(0)
        });
        let resolved = unsigned("acme/x@1.2", "https://github.com/acme/x");
        fetch_pack(&runner, &resolved, home.path(), true).unwrap();
        let (_, args, _) = &runner.calls()[0];
        assert_eq!(
            &args[..8],
            [
                "-c",
                "core.autocrlf=false",
                "clone",
                "--depth",
                "1",
                "--branch",
                "v1.2.0",
                "--"
            ]
        );
    }

    #[test]
    fn signed_pack_is_checked_with_ssh_keygen_and_removed_when_bad() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("r.md"), "# r").unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut resolved = unsigned("acme/x@1.0", &src.path().to_string_lossy());
        resolved.signature = Some("SIG".into());
        resolved.deps = vec![parse_pack_ref("acme/base@1.0").unwrap()];

        // No trust file: fail closed without running anything.
        let runner = FakeRunner::new(|_, _| exit(0));
        let err = fetch_pack(&runner, &resolved, home.path(), true).unwrap_err();
        assert!(err.contains("allowed_signers"), "{err}");
        assert!(runner.calls().is_empty());
        assert!(!resolved.pack.install_path(home.path()).exists());

        std::fs::write(packs_dir(home.path()).join("allowed_signers"), "k").unwrap();
        let dest = fetch_pack(&runner, &resolved, home.path(), false).unwrap();
        let (program, args, stdin) = &runner.calls()[0];
        assert_eq!(program, "ssh-keygen");
        assert_eq!(&args[..2], ["-Y", "verify"]);
        assert!(args.windows(2).any(|w| w == ["-I", "acme"]));
        assert!(args.windows(2).any(|w| w == ["-n", SIGNATURE_NAMESPACE]));
        let digest = pack_digest(&dest).unwrap();
        assert_eq!(
            String::from_utf8_lossy(stdin),
            signed_payload(&resolved.pack, &digest, &resolved.deps)
        );
        let receipt = &list_installed(home.path()).0[0];
        assert_eq!(receipt.deps, vec!["acme/base@1.0.0"]);

        // A signature that does not verify: the fetched content goes away.
        std::fs::remove_dir_all(&dest).unwrap();
        let runner = FakeRunner::new(|_, _| exit(255));
        let err = fetch_pack(&runner, &resolved, home.path(), true).unwrap_err();
        assert!(err.contains("bad signature"), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn real_ssh_signature_round_trip() {
        let runner = crate::pr::SystemCommandRunner;
        let keys = tempfile::tempdir().unwrap();
        let key = keys.path().join("id");
        let key_str = key.to_string_lossy().to_string();
        let keygen =
            |args: &[&str]| crate::pr::CommandRunner::run(&runner, "ssh-keygen", args, keys.path());
        match keygen(&["-q", "-t", "ed25519", "-N", "", "-C", "t", "-f", &key_str]) {
            Ok(out) if out.success() => {}
            // No usable ssh-keygen on this machine: nothing to exercise.
            _ => return,
        }

        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("r.md"), "# r").unwrap();
        let pack = parse_pack_ref("acme/x@1.0").unwrap();
        let payload = signed_payload(&pack, &pack_digest(src.path()).unwrap(), &[]);
        let msg = keys.path().join("payload");
        std::fs::write(&msg, &payload).unwrap();
        let msg_str = msg.to_string_lossy().to_string();
        let signed = keygen(&[
            "-Y",
            "sign",
            "-f",
            &key_str,
            "-n",
            SIGNATURE_NAMESPACE,
            &msg_str,
        ])
        .unwrap();
        assert!(signed.success(), "ssh-keygen -Y sign: {}", signed.stderr);
        let signature = std::fs::read_to_string(keys.path().join("payload.sig")).unwrap();
        let public = std::fs::read_to_string(keys.path().join("id.pub")).unwrap();

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(packs_dir(home.path())).unwrap();
        let signers = packs_dir(home.path()).join("allowed_signers");
        std::fs::write(
            &signers,
            format!("acme namespaces=\"{SIGNATURE_NAMESPACE}\" {public}"),
        )
        .unwrap();

        let mut resolved = unsigned("acme/x@1.0", &src.path().to_string_lossy());
        resolved.signature = Some(signature.clone());
        let dest = fetch_pack(&runner, &resolved, home.path(), false).unwrap();
        let receipt = list_installed(home.path()).0.remove(0);
        verify_installed(&runner, &receipt, home.path(), false).unwrap();

        // Tampering after install breaks the signature.
        std::fs::write(dest.join("r.md"), "# evil").unwrap();
        assert!(verify_installed(&runner, &receipt, home.path(), false).is_err());

        // The same key is not trusted to sign for a different owner.
        let mut other = unsigned("evil/x@1.0", &src.path().to_string_lossy());
        other.signature = Some(signature);
        assert!(fetch_pack(&runner, &other, home.path(), false).is_err());
    }

    #[test]
    fn payload_normalises_and_sorts_deps() {
        let pack = parse_pack_ref("acme/x@1.0").unwrap();
        let deps = [
            parse_pack_ref("z/b@2.1").unwrap(),
            parse_pack_ref("a/c@1.0.4").unwrap(),
        ];
        assert_eq!(
            signed_payload(&pack, "sha256:ab", &deps),
            "wingman-skillpack v1\npack acme/x@1.0.0\ndigest sha256:ab\ndeps a/c@1.0.4 z/b@2.1.0\n"
        );
        assert!(signed_payload(&pack, "sha256:ab", &[]).ends_with("\ndeps\n"));
    }

    #[test]
    fn digest_depends_on_paths_and_contents_but_not_git() {
        let a = tempfile::tempdir().unwrap();
        std::fs::create_dir(a.path().join("tools")).unwrap();
        std::fs::write(a.path().join("tools").join("t.json"), "{}").unwrap();
        std::fs::write(a.path().join("r.md"), "# r").unwrap();
        let before = pack_digest(a.path()).unwrap();
        std::fs::create_dir(a.path().join(".git")).unwrap();
        std::fs::write(a.path().join(".git").join("HEAD"), "x").unwrap();
        assert_eq!(pack_digest(a.path()).unwrap(), before);
        std::fs::write(a.path().join("r.md"), "# R").unwrap();
        assert_ne!(pack_digest(a.path()).unwrap(), before);
    }

    const INDEX: &str = r#"{"packs": {
        "acme/app": [
            {"version": "1.0.0", "source": "https://x/app", "deps": ["acme/base@1.1"]},
            {"version": "1.2.0", "source": "https://x/app", "deps": ["acme/base@1.3"], "signature": "S"}
        ],
        "acme/base": [
            {"version": "1.1.0", "source": "https://x/base"},
            {"version": "1.4.2", "source": "https://x/base", "description": "Base roles"},
            {"version": "2.0.0", "source": "https://x/base"}
        ],
        "acme/old": [
            {"version": "1.0.0", "source": "https://x/old", "deps": ["acme/base@2.0"]}
        ]
    }}"#;

    #[test]
    fn resolve_picks_newest_compatible_versions_transitively() {
        let got = resolve(&index(INDEX), &[parse_pack_ref("acme/app@1.0").unwrap()]).unwrap();
        let specs: Vec<String> = got.iter().map(|r| r.pack.to_string()).collect();
        // base 2.0.0 is newer but outside ^1.3.
        assert_eq!(specs, ["acme/app@1.2.0", "acme/base@1.4.2"]);
        assert_eq!(got[0].signature.as_deref(), Some("S"));
        assert_eq!(got[0].deps, vec![parse_pack_ref("acme/base@1.3").unwrap()]);
    }

    #[test]
    fn resolve_reports_conflicting_requirements() {
        let roots = [
            parse_pack_ref("acme/app@1.0").unwrap(),
            parse_pack_ref("acme/old@1.0").unwrap(),
        ];
        let err = resolve(&index(INDEX), &roots).unwrap_err();
        assert!(err.contains("version conflict for `acme/base`"), "{err}");
        assert!(err.contains("acme/old@1.0.0"), "{err}");
        assert!(err.contains("acme/app@1.2.0"), "{err}");

        let err = resolve(&index(INDEX), &[parse_pack_ref("acme/app@3.0").unwrap()]).unwrap_err();
        assert!(err.contains("version conflict for `acme/app`"), "{err}");
        let err = resolve(&index(INDEX), &[parse_pack_ref("no/such@1.0").unwrap()]).unwrap_err();
        assert!(err.contains("not in the index"), "{err}");
    }

    #[test]
    fn remote_index_may_not_point_at_local_directories() {
        let mut idx =
            index(r#"{"packs": {"acme/x": [{"version": "1.0", "source": "/home/me/.ssh"}]}}"#);
        let roots = [parse_pack_ref("acme/x@1.0").unwrap()];
        assert!(resolve(&idx, &roots).is_ok());
        idx.remote = true;
        let err = resolve(&idx, &roots).unwrap_err();
        assert!(err.contains("non-git source"), "{err}");
    }

    #[test]
    fn search_matches_names_and_descriptions_newest_first() {
        let idx = index(INDEX);
        let hits = search(&idx, "BASE roles");
        assert_eq!(hits.len(), 0, "description of the newest version only");
        let hits = search(&idx, "base");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "acme/base");
        assert_eq!(hits[0].1.version, "2.0.0");
        assert_eq!(search(&idx, "").len(), 3);
    }

    #[test]
    fn load_index_reads_a_directory_or_clones_a_remote() {
        let home = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|_, args| {
            let dest = Path::new(args.last().unwrap());
            std::fs::create_dir_all(dest).unwrap();
            std::fs::write(dest.join(INDEX_FILE), INDEX).unwrap();
            exit(0)
        });

        let local = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join(INDEX_FILE), INDEX).unwrap();
        let idx = load_index(&runner, &local.path().to_string_lossy(), home.path()).unwrap();
        assert!(!idx.remote);
        assert!(runner.calls().is_empty());

        let url = "https://github.com/acme/wingman-packs";
        // Twice: the second load must replace the cached clone, not fail on it.
        load_index(&runner, url, home.path()).unwrap();
        let idx = load_index(&runner, url, home.path()).unwrap();
        assert!(idx.remote);
        assert_eq!(idx.packs.len(), 3);
        assert_eq!(runner.calls().len(), 2);

        assert!(load_index(&runner, "ext::sh -c evil", home.path()).is_err());
        assert!(load_index(&runner, "", home.path()).is_err());
    }

    #[test]
    fn parse_full_spec() {
        let r = parse_pack_ref("wingman-official/rust-developer@1.4").unwrap();
        assert_eq!(r.owner, "wingman-official");
        assert_eq!(r.name, "rust-developer");
        assert_eq!(
            r.version,
            SemVer {
                major: 1,
                minor: 4,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_spec_with_patch() {
        let r = parse_pack_ref("vedantnimbarte/wingman-tui-designer@0.3.2").unwrap();
        assert_eq!(
            r.version,
            SemVer {
                major: 0,
                minor: 3,
                patch: 2
            }
        );
    }

    #[test]
    fn parse_rejects_missing_version() {
        assert!(parse_pack_ref("owner/name").is_err());
    }

    #[test]
    fn parse_rejects_missing_owner() {
        assert!(parse_pack_ref("name@1.0").is_err());
    }

    #[test]
    fn parse_rejects_bad_semver() {
        assert!(parse_pack_ref("o/n@x.y").is_err());
        assert!(parse_pack_ref("o/n@1").is_err());
        assert!(parse_pack_ref("o/n@1.2.3.4").is_err());
    }

    #[test]
    fn semver_satisfies_caret() {
        let installed = SemVer {
            major: 1,
            minor: 5,
            patch: 0,
        };
        assert!(installed.satisfies(&SemVer {
            major: 1,
            minor: 4,
            patch: 0
        }));
        assert!(!installed.satisfies(&SemVer {
            major: 1,
            minor: 6,
            patch: 0
        }));
        assert!(!installed.satisfies(&SemVer {
            major: 2,
            minor: 0,
            patch: 0
        }));
    }

    #[test]
    fn slug_and_install_path() {
        let r = parse_pack_ref("acme/sec-reviewer@2.0").unwrap();
        assert_eq!(r.slug(), "acme__sec-reviewer@2.0.0");
        let p = r.install_path(Path::new("/home/u"));
        assert!(p.ends_with("acme__sec-reviewer@2.0.0"));
        assert!(p.to_string_lossy().contains("packs"));
    }

    #[test]
    fn parse_list_separates_ok_and_errors() {
        let specs = vec![
            "a/b@1.0".to_string(),
            "broken".to_string(),
            "c/d@2.1".to_string(),
        ];
        let (ok, errs) = parse_pack_list(&specs);
        assert_eq!(ok.len(), 2);
        assert_eq!(errs.len(), 1);
    }
}
