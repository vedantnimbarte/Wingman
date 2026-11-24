//! The I/O half of telemetry intake: fetch Sentry, PostHog and custom
//! sources, then file each finding as a board card. Mapping lives in
//! `wingman_autonomous::telemetry`, dedup in `wingman_board::intake`.
//!
//! Runs from the daemon loop once per poll. It never dispatches anything.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use wingman_autonomous::telemetry::{self, Coverage, Finding};
use wingman_board::{BoardStore, Filed};
use wingman_config::{CustomSourceConfig, PilotDaemonConfig};

/// Sources this module handles. They file cards instead of going through
/// the daemon's score/dispatch path.
pub const SOURCES: &[&str] = &["sentry", "posthog", "custom"];

const TIMEOUT: Duration = Duration::from_secs(30);

/// `reqwest` errors carry the URL; a custom source's URL may hold a key in
/// its query string, so it is dropped before the error is logged.
fn http_err(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

async fn send(req: reqwest::RequestBuilder) -> Result<Value, String> {
    let resp = req.send().await.map_err(http_err)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.json().await.map_err(http_err)
}

fn require(fields: &[(&str, &str)]) -> Result<(), String> {
    let missing: Vec<&str> = fields
        .iter()
        .filter(|(_, v)| v.trim().is_empty() || v.starts_with("${"))
        .map(|(k, _)| *k)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("not configured: {}", missing.join(", ")))
    }
}

async fn sentry(client: &reqwest::Client, cfg: &PilotDaemonConfig) -> Result<Vec<Finding>, String> {
    let s = cfg
        .sentry
        .as_ref()
        .ok_or("no [pilot.daemon.sentry] section")?;
    require(&[
        ("org", &s.org),
        ("project", &s.project),
        ("token", &s.token),
    ])?;
    let json = send(client.get(telemetry::sentry_url(s)).bearer_auth(&s.token)).await?;
    Ok(telemetry::sentry_findings(&json))
}

async fn posthog(
    client: &reqwest::Client,
    cfg: &PilotDaemonConfig,
) -> Result<Vec<Finding>, String> {
    let p = cfg
        .posthog
        .as_ref()
        .ok_or("no [pilot.daemon.posthog] section")?;
    require(&[("project_id", &p.project_id), ("token", &p.token)])?;
    let url = telemetry::posthog_url(p);
    let mut out = Vec::new();
    for (signal, body) in telemetry::posthog_queries(p) {
        // One failed query fails the source: a partial read reported as
        // complete would hide exactly the signal that broke.
        let json = send(client.post(&url).bearer_auth(&p.token).json(&body))
            .await
            .map_err(|e| format!("{signal}: {e}"))?;
        out.extend(telemetry::posthog_findings(&signal, &json, p));
    }
    Ok(out)
}

async fn custom(
    client: &reqwest::Client,
    c: &CustomSourceConfig,
    root: &Path,
) -> Result<Vec<Finding>, String> {
    let json = match c.kind.as_str() {
        "http" => {
            require(&[("url", &c.url)])?;
            let mut req = client.get(&c.url);
            if let Some(t) = c.token.as_deref().filter(|t| !t.is_empty()) {
                req = req.bearer_auth(t);
            }
            send(req).await?
        }
        "cli" => {
            let (prog, args) = c.command.split_first().ok_or("empty command")?;
            // Run directly, never through a shell.
            let out = tokio::time::timeout(
                TIMEOUT,
                tokio::process::Command::new(prog)
                    .args(args)
                    .current_dir(root)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| format!("timed out after {}s", TIMEOUT.as_secs()))?
            .map_err(|e| format!("{prog}: {e}"))?;
            if !out.status.success() {
                return Err(format!("{prog} exited {}", out.status));
            }
            serde_json::from_slice(&out.stdout).map_err(|e| format!("bad JSON: {e}"))?
        }
        other => return Err(format!("unknown kind {other:?} (http or cli)")),
    };
    Ok(telemetry::custom_findings(c, &json))
}

/// Every enabled telemetry source, fetched. `(name, coverage, findings)`.
async fn collect(cfg: &PilotDaemonConfig, root: &Path) -> Vec<(String, Coverage, Vec<Finding>)> {
    wingman_core::ensure_tls_provider();
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return vec![(
                "telemetry".into(),
                Coverage::Unavailable(http_err(e)),
                Vec::new(),
            )]
        }
    };
    let on = |name: &str| cfg.sources.iter().any(|s| s == name);
    let mut results = Vec::new();
    let mut push = |name: String, r: Result<Vec<Finding>, String>| match r {
        Ok(f) => results.push((name, Coverage::Read(f.len()), f)),
        Err(e) => results.push((name, Coverage::Unavailable(e), Vec::new())),
    };
    if on("sentry") {
        push("sentry".into(), sentry(&client, cfg).await);
    }
    if on("posthog") {
        push("posthog".into(), posthog(&client, cfg).await);
    }
    if on("custom") {
        if cfg.custom.is_empty() {
            push(
                "custom".into(),
                Err("no [[pilot.daemon.custom]] entries".into()),
            );
        }
        for c in &cfg.custom {
            push(format!("custom:{}", c.name), custom(&client, c, root).await);
        }
    }
    results
}

/// One intake pass: fetch, then file onto this project's board.
pub async fn intake_cycle(cfg: &PilotDaemonConfig, root: &Path, dry_run: bool, n: usize) {
    if !cfg.sources.iter().any(|s| SOURCES.contains(&s.as_str())) {
        return;
    }
    let results = collect(cfg, root).await;
    let store = match BoardStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[pilot] daemon cycle {n}: telemetry: cannot open the board: {e}");
            return;
        }
    };
    let project_id = match store.touch_project(root) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("[pilot] daemon cycle {n}: telemetry: cannot register project: {e}");
            return;
        }
    };
    let tag = if dry_run {
        "[dry-run] would file"
    } else {
        "filed"
    };
    for (name, coverage, findings) in results {
        match coverage {
            Coverage::Unavailable(why) => {
                eprintln!("[pilot] daemon cycle {n}: {name}: unavailable — {why}");
                continue;
            }
            Coverage::Read(count) => {
                eprintln!("[pilot] daemon cycle {n}: {name}: read {count}");
            }
        }
        for f in findings {
            match store.file_finding(&project_id, &f, dry_run) {
                Ok(Filed::New(card)) => {
                    eprintln!(
                        "[pilot]   {tag} {} {} ({})",
                        card.short(),
                        card.title,
                        f.fingerprint
                    )
                }
                Ok(Filed::Recurrence { card, previous }) => eprintln!(
                    "[pilot]   {tag} {} {} — recurrence of {} ({})",
                    card.short(),
                    card.title,
                    previous.short(),
                    f.fingerprint
                ),
                Ok(Filed::SeenAgain { .. } | Filed::Archived(_)) => {}
                Err(e) => eprintln!("[pilot]   failed to file {}: {e}", f.fingerprint),
            }
        }
    }
}
