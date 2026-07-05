//! J12 — skill packs (shareable, versioned agent definitions).
//!
//! Role definitions in `~/.wingman/agents/<role>.md` become shareable,
//! semver-pinned bundles: a directory of role markdown + lessons + tool
//! registrations + acceptance templates, installed from a git repo or
//! local path. Configured via `[pilot.skills].packs` as
//! `owner/name@semver` strings.
//!
//! This module parses + validates those specs and resolves install paths.
//! Fetching/unpacking is the installer's job; this is the pure spec layer.

use std::path::{Path, PathBuf};

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
        home.join(".wingman").join("packs").join(self.slug())
    }
}

/// A minimal semantic version (`major.minor` or `major.minor.patch`).
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
