//! R6 — security pass in the PR pipeline.
//!
//! Runs before E8's auto-merge gate. Auto-merging without this is
//! reckless: a worker can plausibly paste a leaked key into a fixture, or
//! pull in a GPL dependency, and the per-task reviewer (E7) won't reliably
//! catch either. Four checks, all surfaced as [`SecurityFinding`]s on the
//! shared [`Severity`] scale so the same `block_severity` gate applies:
//!
//! 1. **Secrets scan** — built-in heuristic (known key prefixes + Shannon
//!    entropy) over added diff lines, so a scan always runs. When `gitleaks`
//!    is on PATH the pipeline runs it too and [`parse_gitleaks_report`]
//!    folds its report in.
//! 2. **Dependency audit** — [`parse_cargo_audit`] folds `cargo audit
//!    --json` output into findings.
//! 3. **License scan** — [`parse_cargo_lock`] / [`parse_package_lock`] find
//!    the dependencies a run added to a lockfile, and [`scan_licenses`] checks
//!    their SPDX licenses against the allow/deny policy.
//!
//! Findings are rendered by [`render_report`] (posted as a PR comment) and
//! the report can [`SecurityReport::blocks_merge`] the auto-merge gate.
//! External tools that were absent or failed are recorded in
//! [`SecurityReport::notes`] rather than silently skipped.

use std::collections::BTreeSet;

use crate::severity::{max_severity, Severity};

/// One security finding, on the shared severity scale. Internal to the
/// pipeline — not serialised, so no serde derive.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityFinding {
    pub severity: Severity,
    /// Short category: "secret", "vulnerability", "license".
    pub kind: String,
    pub message: String,
    pub file: Option<String>,
}

/// Aggregated result of a security pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SecurityReport {
    pub findings: Vec<SecurityFinding>,
    /// What each external scanner did: ran, was not installed, or failed.
    /// A skipped scanner is not a clean result, so the summary says so.
    pub notes: Vec<String>,
}

impl SecurityReport {
    pub fn max_severity(&self) -> Option<Severity> {
        max_severity(&self.findings, |f| f.severity)
    }

    /// True when any finding meets or exceeds the configured gate.
    pub fn blocks_merge(&self, block_gate: Severity) -> bool {
        self.max_severity()
            .is_some_and(|s| s.meets_or_exceeds(block_gate))
    }

    pub fn extend(&mut self, more: impl IntoIterator<Item = SecurityFinding>) {
        self.findings.extend(more);
    }
}

// ---------------------------------------------------------------------------
// 1. Secrets scan
// ---------------------------------------------------------------------------

/// Known high-confidence credential prefixes → human label.
const KEY_PREFIXES: &[(&str, &str)] = &[
    ("AKIA", "AWS access key id"),
    ("ASIA", "AWS temporary access key"),
    ("ghp_", "GitHub personal access token"),
    ("gho_", "GitHub OAuth token"),
    ("ghs_", "GitHub server token"),
    ("github_pat_", "GitHub fine-grained PAT"),
    ("xoxb-", "Slack bot token"),
    ("xoxp-", "Slack user token"),
    ("sk-", "OpenAI / Anthropic style secret key"),
    ("AIza", "Google API key"),
    ("ya29.", "Google OAuth token"),
    ("glpat-", "GitLab personal access token"),
    ("-----BEGIN", "private key block"),
];

/// Shannon entropy in bits per character of `s`.
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0u32) += 1;
    }
    let len = s.chars().count() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// True when `token` looks like a high-entropy credential: long enough,
/// mostly credential-shaped characters, and high per-char entropy. Tuned
/// to flag base64/hex secrets while ignoring prose and short identifiers.
pub fn looks_like_secret(token: &str) -> bool {
    let t = token.trim_matches(|c: char| {
        !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '+' && c != '/'
    });
    if t.len() < 20 {
        return false;
    }
    let credentialish = t
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
        .count();
    // Require the token to be almost entirely credential-shaped.
    if (credentialish as f64) < 0.95 * t.len() as f64 {
        return false;
    }
    // Must contain at least one digit and one letter (rules out
    // "aaaaaaaaaaaaaaaaaaaa" and "--------------------").
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
    if !(has_digit && has_alpha) {
        return false;
    }
    shannon_entropy(t) >= 3.5
}

/// Heuristic that a line assigns a credential, e.g.
/// `api_key = "..."` / `SECRET: ...`. Used to raise severity when a
/// high-entropy token sits next to a credential-ish key name.
fn line_mentions_credential(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    [
        "secret",
        "token",
        "passwd",
        "password",
        "api_key",
        "apikey",
        "access_key",
        "private_key",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// Scan added diff lines for secrets. `added` is a list of
/// `(file, line_text)` pairs (only the `+` lines of a diff, sans the `+`).
pub fn scan_secrets(added: &[(String, String)]) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    for (file, line) in added {
        let mut prefix_hit = false;
        // Known prefixes — high confidence.
        for (prefix, label) in KEY_PREFIXES {
            // Find the prefix as the start of a token (preceded by a
            // non-token char or start of line).
            if let Some(pos) = line.find(prefix) {
                let at_boundary = pos == 0
                    || !line
                        .as_bytes()
                        .get(pos - 1)
                        .map(|b| b.is_ascii_alphanumeric() || *b == b'_')
                        .unwrap_or(false);
                if at_boundary {
                    findings.push(SecurityFinding {
                        severity: Severity::Critical,
                        kind: "secret".into(),
                        message: format!("possible {label} (prefix `{prefix}`)"),
                        file: Some(file.clone()),
                    });
                    prefix_hit = true;
                    break; // one finding per line is enough
                }
            }
        }
        if prefix_hit {
            // Already flagged with high confidence; don't double-count the
            // same line via the entropy heuristic.
            continue;
        }
        // Entropy-based — medium/high confidence.
        for token in line.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')')
        }) {
            if looks_like_secret(token) {
                let sev = if line_mentions_credential(line) {
                    Severity::High
                } else {
                    Severity::Medium
                };
                findings.push(SecurityFinding {
                    severity: sev,
                    kind: "secret".into(),
                    message: "high-entropy string resembling a credential".into(),
                    file: Some(file.clone()),
                });
                break;
            }
        }
    }
    findings
}

/// Parse a gitleaks JSON report (`--report-format json`) into findings.
/// gitleaks rules are specific, so every hit is [`Severity::High`]. Run it
/// with `--redact`; the secret itself is never copied into the message
/// either way, because the message ends up in a PR comment.
pub fn parse_gitleaks_report(json: &str) -> Result<Vec<SecurityFinding>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid gitleaks report: {e}"))?;
    let list = v
        .as_array()
        .ok_or_else(|| "invalid gitleaks report: expected an array".to_string())?;
    Ok(list
        .iter()
        .map(|item| {
            let s = |k: &str| item.get(k).and_then(|x| x.as_str()).unwrap_or("");
            let line = item.get("StartLine").and_then(|x| x.as_u64()).unwrap_or(0);
            SecurityFinding {
                severity: Severity::High,
                kind: "secret".into(),
                message: format!(
                    "gitleaks: {} (rule `{}`, line {line})",
                    s("Description"),
                    s("RuleID")
                ),
                file: Some(s("File").to_string()).filter(|f| !f.is_empty()),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 2. Dependency audit (cargo audit --json)
// ---------------------------------------------------------------------------

/// Parse `cargo audit --json` output into findings. Tolerant of the exact
/// shape: reads `vulnerabilities.list[]`, extracting advisory id/title and
/// mapping a CVSS score (when present) to a [`Severity`]; defaults to
/// [`Severity::High`] for any advisory without a score (audit only reports
/// real vulnerabilities).
pub fn parse_cargo_audit(json: &str) -> Result<Vec<SecurityFinding>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid cargo-audit json: {e}"))?;
    let mut findings = Vec::new();
    let list = v
        .pointer("/vulnerabilities/list")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();
    for item in list {
        let advisory = item
            .get("advisory")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let id = advisory
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or("UNKNOWN");
        let title = advisory
            .get("title")
            .and_then(|s| s.as_str())
            .unwrap_or("(no title)");
        let pkg = item
            .pointer("/package/name")
            .and_then(|s| s.as_str())
            .unwrap_or("(unknown crate)");
        let severity = advisory
            .get("cvss")
            .and_then(|c| c.as_str())
            .and_then(parse_cvss_vector_score)
            .map(cvss_score_to_severity)
            .unwrap_or(Severity::High);
        findings.push(SecurityFinding {
            severity,
            kind: "vulnerability".into(),
            message: format!("{id}: {title} (in `{pkg}`)"),
            file: Some("Cargo.lock".into()),
        });
    }
    Ok(findings)
}

/// Extract the base score from a CVSS vector if it carries one, or parse a
/// bare numeric score. cargo-audit's `cvss` field is usually a vector
/// string; we only need a coarse score so we look for a trailing number.
fn parse_cvss_vector_score(cvss: &str) -> Option<f64> {
    // Bare number?
    if let Ok(n) = cvss.trim().parse::<f64>() {
        return Some(n);
    }
    None
}

fn cvss_score_to_severity(score: f64) -> Severity {
    match score {
        s if s >= 9.0 => Severity::Critical,
        s if s >= 7.0 => Severity::High,
        s if s >= 4.0 => Severity::Medium,
        s if s > 0.0 => Severity::Low,
        _ => Severity::Info,
    }
}

// ---------------------------------------------------------------------------
// 3. License scan
// ---------------------------------------------------------------------------

/// Check dependency licenses against the allow/deny policy. `deps` is a
/// list of `(package_name, spdx_license)` from `lockfile`, which findings are
/// attributed to.
///
/// A license term is acceptable when it is not in `denied` and, if `allowed`
/// is non-empty, is in `allowed`. SPDX `OR` / `/` expressions pass if *any*
/// alternative is acceptable; `AND` expressions require *all*. A failing
/// expression that names a denied license is [`Severity::Critical`]; one that
/// is merely not allowlisted is [`Severity::High`].
pub fn scan_licenses(
    deps: &[(String, String)],
    lockfile: &str,
    allowed: &[String],
    denied: &[String],
) -> Vec<SecurityFinding> {
    let listed = |list: &[String], lic: &str| list.iter().any(|a| a.eq_ignore_ascii_case(lic));
    let acceptable =
        |lic: &str| !listed(denied, lic) && (allowed.is_empty() || listed(allowed, lic));
    let mut findings = Vec::new();
    for (name, license) in deps {
        if license.trim().is_empty() {
            findings.push(SecurityFinding {
                severity: Severity::Medium,
                kind: "license".into(),
                message: format!("`{name}` has no declared license"),
                file: Some(lockfile.into()),
            });
            continue;
        }
        let (terms, any) = if license.contains(" OR ") || license.contains('/') {
            (split_spdx(license, &["OR", "/"]), true)
        } else if license.contains(" AND ") {
            (split_spdx(license, &["AND"]), false)
        } else {
            (vec![license.trim().to_string()], false)
        };
        let ok = if any {
            terms.iter().any(|l| acceptable(l))
        } else {
            terms.iter().all(|l| acceptable(l))
        };
        if ok {
            continue;
        }
        let (severity, why) = if terms.iter().any(|l| listed(denied, l)) {
            (Severity::Critical, "denied")
        } else {
            (Severity::High, "non-allowlisted")
        };
        findings.push(SecurityFinding {
            severity,
            kind: "license".into(),
            message: format!("`{name}` uses {why} license `{license}`"),
            file: Some(lockfile.into()),
        });
    }
    findings
}

/// `(name, version)` of every registry/git package in a `Cargo.lock`.
/// Workspace members and path dependencies carry no `source` and are left
/// out: they are the project itself, not dependencies it pulled in.
/// `Cargo.lock` holds no license data; the pipeline gets that from
/// `cargo metadata`, which resolves against the same lockfile.
pub fn parse_cargo_lock(text: &str) -> BTreeSet<(String, String)> {
    text.split("[[package]]")
        .skip(1)
        .filter_map(|block| {
            let field = |key: &str| {
                block.lines().find_map(|l| {
                    let value = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?;
                    Some(value.trim().trim_matches('"').to_string())
                })
            };
            field("source")?;
            Some((field("name")?, field("version")?))
        })
        .collect()
}

/// `(name, version, license)` of every installed package in an npm
/// `package-lock.json` (lockfileVersion 2 or 3). Workspace links are left
/// out for the same reason as in [`parse_cargo_lock`]. lockfileVersion 1 has
/// no `packages` map and no license data, which is an error rather than an
/// empty (and falsely clean) list.
pub fn parse_package_lock(json: &str) -> Result<Vec<(String, String, String)>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid package-lock.json: {e}"))?;
    let packages = v
        .get("packages")
        .and_then(|p| p.as_object())
        .ok_or_else(|| {
            "package-lock.json has no `packages` map (lockfileVersion 1 carries no license data)"
                .to_string()
        })?;
    Ok(packages
        .iter()
        .filter(|(_, entry)| entry.get("link").and_then(|l| l.as_bool()) != Some(true))
        .filter_map(|(key, entry)| {
            let name = &key[key.rfind("node_modules/")? + "node_modules/".len()..];
            let s = |k: &str| {
                entry
                    .get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            Some((name.to_string(), s("version"), s("license")))
        })
        .collect())
}

fn split_spdx(expr: &str, seps: &[&str]) -> Vec<String> {
    let mut parts = vec![expr.to_string()];
    for sep in seps {
        let pat = format!(" {sep} ");
        parts = parts
            .iter()
            .flat_map(|p| {
                if *sep == "/" {
                    p.split('/').map(|s| s.to_string()).collect::<Vec<_>>()
                } else {
                    p.split(&pat).map(|s| s.to_string()).collect::<Vec<_>>()
                }
            })
            .collect();
    }
    parts
        .into_iter()
        .map(|p| {
            p.trim()
                .trim_matches(|c| c == '(' || c == ')')
                .trim()
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Report rendering
// ---------------------------------------------------------------------------

/// Render a `security.md` artifact from the report.
pub fn render_report(report: &SecurityReport, block_gate: Severity) -> String {
    let mut out = String::from("# Security pass\n\n");
    if report.findings.is_empty() {
        out.push_str("✅ No findings.\n");
    } else {
        render_findings(&mut out, report, block_gate);
    }
    if !report.notes.is_empty() {
        out.push_str("\n## Scanners\n\n");
        for n in &report.notes {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out
}

fn render_findings(out: &mut String, report: &SecurityReport, block_gate: Severity) {
    let blocking = report.blocks_merge(block_gate);
    out.push_str(&format!(
        "{} {} finding(s); highest severity **{}**. Auto-merge {}.\n\n",
        if blocking { "⛔" } else { "⚠️" },
        report.findings.len(),
        report.max_severity().map(|s| s.as_str()).unwrap_or("none"),
        if blocking { "**blocked**" } else { "permitted" }
    ));
    for f in &report.findings {
        let loc = f
            .file
            .as_deref()
            .map(|p| format!(" `{p}`"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- [{}] ({}){} {}\n",
            f.severity, f.kind, loc, f.message
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(file: &str, text: &str) -> (String, String) {
        (file.to_string(), text.to_string())
    }

    #[test]
    fn detects_aws_key_prefix() {
        let added = vec![line("config.rs", r#"let k = "AKIAIOSFODNN7EXAMPLE";"#)];
        let f = scan_secrets(&added);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
        assert!(f[0].message.contains("AWS"));
    }

    #[test]
    fn detects_github_token() {
        let added = vec![line(
            ".env",
            "GITHUB_TOKEN=ghp_1234567890abcdefghijklmnopqrstuvwxyz",
        )];
        let f = scan_secrets(&added);
        assert!(f.iter().any(|x| x.message.contains("GitHub")));
    }

    #[test]
    fn entropy_flag_for_high_entropy_assignment() {
        let added = vec![line(
            "app.rs",
            r#"let api_key = "x9Kd82mZq7Lp03Wn5Tb1Yc4Rf6Hj8Gv";"#,
        )];
        let f = scan_secrets(&added);
        assert!(!f.is_empty());
        // Near a credential keyword → High.
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn ignores_normal_prose() {
        let added = vec![
            line(
                "README.md",
                "This is a perfectly ordinary sentence of documentation.",
            ),
            line("main.rs", "let total = items.iter().map(|x| x.cost).sum();"),
        ];
        assert!(scan_secrets(&added).is_empty());
    }

    #[test]
    fn ignores_low_entropy_repeats() {
        assert!(!looks_like_secret("aaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!looks_like_secret("------------------------"));
    }

    #[test]
    fn entropy_increases_with_randomness() {
        let low = shannon_entropy("aaaaaaaa");
        let high = shannon_entropy("a1B2c3D4");
        assert!(high > low);
    }

    #[test]
    fn parse_cargo_audit_extracts_advisories() {
        let json = r#"{
            "vulnerabilities": {
                "found": true,
                "count": 1,
                "list": [
                    {
                        "advisory": {"id": "RUSTSEC-2021-0001", "title": "buffer overflow"},
                        "package": {"name": "badcrate", "version": "0.1.0"}
                    }
                ]
            }
        }"#;
        let f = parse_cargo_audit(json).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "vulnerability");
        assert!(f[0].message.contains("RUSTSEC-2021-0001"));
        assert!(f[0].message.contains("badcrate"));
        // No CVSS → defaults to High.
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn parse_cargo_audit_empty_is_clean() {
        let json = r#"{"vulnerabilities": {"found": false, "count": 0, "list": []}}"#;
        assert!(parse_cargo_audit(json).unwrap().is_empty());
    }

    #[test]
    fn license_allowlist_passes_mit() {
        let deps = vec![("serde".to_string(), "MIT".to_string())];
        let allowed = vec!["MIT".to_string(), "Apache-2.0".to_string()];
        assert!(scan_licenses(&deps, "Cargo.lock", &allowed, &[]).is_empty());
    }

    #[test]
    fn license_or_expression_passes_if_any_allowed() {
        let deps = vec![("foo".to_string(), "MIT OR Apache-2.0".to_string())];
        let allowed = vec!["Apache-2.0".to_string()];
        assert!(scan_licenses(&deps, "Cargo.lock", &allowed, &[]).is_empty());
    }

    #[test]
    fn license_slash_expression_passes_if_any_allowed() {
        let deps = vec![("foo".to_string(), "MIT/Apache-2.0".to_string())];
        let allowed = vec!["MIT".to_string()];
        assert!(scan_licenses(&deps, "Cargo.lock", &allowed, &[]).is_empty());
    }

    #[test]
    fn license_gpl_is_flagged() {
        let deps = vec![("copyleft".to_string(), "GPL-3.0".to_string())];
        let allowed = vec!["MIT".to_string()];
        let f = scan_licenses(&deps, "Cargo.lock", &allowed, &[]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn license_missing_is_flagged_medium() {
        let deps = vec![("mystery".to_string(), "".to_string())];
        let f = scan_licenses(&deps, "Cargo.lock", &["MIT".to_string()], &[]);
        assert_eq!(f[0].severity, Severity::Medium);
    }

    #[test]
    fn license_and_requires_all_allowed() {
        let deps = vec![("dual".to_string(), "MIT AND GPL-3.0".to_string())];
        let allowed = vec!["MIT".to_string()];
        // GPL not allowed → AND fails.
        assert_eq!(scan_licenses(&deps, "Cargo.lock", &allowed, &[]).len(), 1);
    }

    #[test]
    fn report_blocks_at_gate() {
        let mut r = SecurityReport::default();
        r.extend(scan_licenses(
            &[("copyleft".to_string(), "GPL-3.0".to_string())],
            "Cargo.lock",
            &["MIT".to_string()],
            &[],
        ));
        assert!(r.blocks_merge(Severity::Medium));
        assert!(!r.blocks_merge(Severity::Critical));
    }

    #[test]
    fn render_report_clean_and_dirty() {
        let clean = SecurityReport::default();
        assert!(render_report(&clean, Severity::Medium).contains("No findings"));

        let mut dirty = SecurityReport::default();
        dirty.extend(scan_secrets(&[line("x", r#""AKIAIOSFODNN7EXAMPLE""#)]));
        let md = render_report(&dirty, Severity::Medium);
        assert!(md.contains("blocked"));
        assert!(md.contains("critical"));
    }

    /// A denied license fails even when the allowlist also names it, is
    /// critical, and an `OR` expression still passes through an acceptable
    /// alternative. With an empty allowlist, only the denylist applies.
    #[test]
    fn license_denylist_wins_and_is_critical() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        let allowed = vec!["MIT".to_string(), "GPL-3.0".to_string()];
        let denied = vec!["GPL-3.0".to_string()];

        let f = scan_licenses(
            &[s("copyleft", "GPL-3.0")],
            "package-lock.json",
            &allowed,
            &denied,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
        assert!(f[0].message.contains("denied"), "{}", f[0].message);
        assert_eq!(f[0].file.as_deref(), Some("package-lock.json"));

        let dual = [s("dual", "MIT OR GPL-3.0")];
        assert!(scan_licenses(&dual, "Cargo.lock", &allowed, &denied).is_empty());
        let both = [s("both", "MIT AND GPL-3.0")];
        assert_eq!(
            scan_licenses(&both, "Cargo.lock", &allowed, &denied)[0].severity,
            Severity::Critical
        );

        // Deny-only policy: anything not denied passes.
        let deps = [s("a", "Zlib"), s("b", "AGPL-3.0")];
        let f = scan_licenses(&deps, "Cargo.lock", &[], &["AGPL-3.0".to_string()]);
        assert_eq!(f.len(), 1);
        assert!(f[0].message.contains("`b`"));
    }

    #[test]
    fn parse_cargo_lock_keeps_only_sourced_packages() {
        let lock = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
dependencies = [
 "serde_derive",
]

[[package]]
name = "wingman-core"
version = "0.4.0"
dependencies = [
 "serde",
]

[[package]]
name = "forked"
version = "0.1.0"
source = "git+https://example.com/forked#deadbeef"
"#;
        let pkgs = parse_cargo_lock(lock);
        assert_eq!(pkgs.len(), 2, "{pkgs:?}");
        assert!(pkgs.contains(&("serde".into(), "1.0.200".into())));
        assert!(pkgs.contains(&("forked".into(), "0.1.0".into())));
    }

    #[test]
    fn parse_package_lock_reads_nested_packages_and_licenses() {
        let lock = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "app", "version": "1.0.0"},
                "node_modules/react": {"version": "18.3.1", "license": "MIT"},
                "node_modules/a/node_modules/@scope/b": {"version": "2.0.0", "license": "GPL-3.0"},
                "node_modules/local": {"resolved": "packages/local", "link": true},
                "packages/local": {"version": "0.0.1"}
            }
        }"#;
        let mut pkgs = parse_package_lock(lock).unwrap();
        pkgs.sort();
        assert_eq!(
            pkgs,
            vec![
                ("@scope/b".into(), "2.0.0".into(), "GPL-3.0".into()),
                ("react".into(), "18.3.1".into(), "MIT".into()),
            ]
        );
        // lockfileVersion 1 cannot be scanned and says so.
        assert!(parse_package_lock(r#"{"lockfileVersion": 1, "dependencies": {}}"#).is_err());
    }

    #[test]
    fn parse_gitleaks_report_keeps_the_secret_out_of_the_message() {
        let json = r#"[{
            "Description": "AWS Access Key",
            "RuleID": "aws-access-token",
            "File": "src/config.rs",
            "StartLine": 12,
            "Secret": "AKIAIOSFODNN7EXAMPLE",
            "Match": "key = AKIAIOSFODNN7EXAMPLE"
        }]"#;
        let f = parse_gitleaks_report(json).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].file.as_deref(), Some("src/config.rs"));
        assert!(f[0].message.contains("aws-access-token"));
        assert!(!f[0].message.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(parse_gitleaks_report("[]").unwrap().is_empty());
        assert!(parse_gitleaks_report("not json").is_err());
    }

    /// Skipped scanners are listed in the summary, clean or not, so "no
    /// findings" is never mistaken for "every scanner ran".
    #[test]
    fn render_report_lists_scanner_notes() {
        let report = SecurityReport {
            findings: Vec::new(),
            notes: vec!["gitleaks not found on PATH; skipped".into()],
        };
        let md = render_report(&report, Severity::Medium);
        assert!(md.contains("No findings"));
        assert!(md.contains("## Scanners"));
        assert!(md.contains("gitleaks not found on PATH"));
    }
}
