//! Telemetry intake: Sentry, PostHog and custom JSON sources, mapped to
//! [`Finding`]s the daemon files as board cards.
//!
//! This module is the pure half: request shapes in, JSON out → findings. The
//! HTTP calls live in `wingman-cli` (this crate has no HTTP client) and the
//! board filing in `wingman-board`. Telemetry never dispatches a run — an
//! error count is not a spec, so a person promotes the card.
//!
//! Every string in a finding comes from outside (exception messages, URLs,
//! issue titles). It ends up in a card's goal, framed as evidence; the card
//! is only dispatched by a human, who reads it first.

use serde_json::Value;
use wingman_config::{CustomSourceConfig, PostHogSourceConfig, SentrySourceConfig};

/// One problem worth a card.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// `sentry`, `posthog`, or `custom:<name>`.
    pub source: String,
    /// Stable identity across polls, e.g. `sentry:4411`. Dedup key.
    pub fingerprint: String,
    pub title: String,
    /// Evidence as markdown: links, counts, dates. Never secrets.
    pub body: String,
    /// RFC-3339 time the problem was last observed, when the source says.
    /// A finding whose card was archived is refiled only if this is newer
    /// than the archive.
    pub last_seen: Option<String>,
}

/// What one source returned this poll. Unreachable is never reported as
/// empty: a quiet source and a broken one must not look the same.
#[derive(Debug, Clone, PartialEq)]
pub enum Coverage {
    Read(usize),
    Unavailable(String),
}

/// Most findings one source may file per poll.
pub const MAX_FINDINGS: usize = 20;

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Numbers arrive as JSON numbers or as strings (Sentry's `count`).
fn num(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|t| t.parse().ok()))
        .unwrap_or(0.0)
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------- Sentry

/// `GET` this URL with `Authorization: Bearer <token>`.
pub fn sentry_url(cfg: &SentrySourceConfig) -> String {
    format!(
        "{}/api/0/projects/{}/{}/issues/?query=is%3Aunresolved%20lastSeen%3A-{}d&sort=freq&limit={}",
        cfg.base_url.trim_end_matches('/'),
        cfg.org,
        cfg.project,
        cfg.window_days.max(1),
        MAX_FINDINGS
    )
}

/// Sentry's issue list → one finding per unresolved issue.
pub fn sentry_findings(json: &Value) -> Vec<Finding> {
    json.as_array()
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let id = s(it, "id");
            if id.is_empty() {
                return None;
            }
            let title = one_line(s(it, "title"), 100);
            let culprit = s(it, "culprit");
            let mut body = format!(
                "- Sentry issue: {} ({})\n- Events: {}, users: {}\n- First seen: {}, last seen: {}\n",
                s(it, "permalink"),
                s(it, "shortId"),
                num(it.get("count").unwrap_or(&Value::Null)),
                num(it.get("userCount").unwrap_or(&Value::Null)),
                s(it, "firstSeen"),
                s(it, "lastSeen"),
            );
            if !culprit.is_empty() {
                body.push_str(&format!("- Culprit: `{culprit}`\n"));
            }
            Some(Finding {
                source: "sentry".into(),
                fingerprint: format!("sentry:{id}"),
                title: if culprit.is_empty() {
                    format!("Fix {title}")
                } else {
                    format!("Fix {title} in {}", one_line(culprit, 60))
                },
                body,
                last_seen: Some(s(it, "lastSeen").to_string()).filter(|t| !t.is_empty()),
            })
        })
        .take(MAX_FINDINGS)
        .collect()
}

// ---------------------------------------------------------------- PostHog

/// LCP p75 above this (ms) files a slow-page card. Google's "poor" band.
const LCP_POOR_MS: f64 = 4000.0;
const INP_POOR_MS: f64 = 500.0;
const CLS_POOR: f64 = 0.25;
/// Web vitals need this many samples before a p75 means anything.
const MIN_VITALS_SAMPLES: f64 = 50.0;

/// `POST {host}/api/projects/{id}/query/` with this body.
pub fn posthog_url(cfg: &PostHogSourceConfig) -> String {
    format!(
        "{}/api/projects/{}/query/",
        cfg.host.trim_end_matches('/'),
        cfg.project_id
    )
}

/// HogQL string literal. Config values only, but quoted properly anyway.
fn hogql_str(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// One HogQL query per enabled signal: `(signal, query body)`.
pub fn posthog_queries(cfg: &PostHogSourceConfig) -> Vec<(String, Value)> {
    let days = cfg.window_days.max(1);
    let min = cfg.min_users.max(1);
    let window = format!("timestamp > now() - INTERVAL {days} DAY");
    let mut out = Vec::new();
    let on = |name: &str| cfg.signals.iter().any(|s| s == name);
    if on("exception") {
        out.push((
            "exception".into(),
            format!(
                "SELECT properties.$exception_issue_id AS id, \
                 any(properties.$exception_types) AS types, \
                 any(properties.$exception_values) AS vals, \
                 count() AS n, count(DISTINCT person_id) AS users, max(timestamp) AS last \
                 FROM events WHERE event = '$exception' AND {window} AND id IS NOT NULL \
                 GROUP BY id HAVING users >= {min} ORDER BY users DESC LIMIT {MAX_FINDINGS}"
            ),
        ));
    }
    if on("failure_events") && !cfg.failure_events.is_empty() {
        let names: Vec<String> = cfg.failure_events.iter().map(|e| hogql_str(e)).collect();
        out.push((
            "failure_events".into(),
            format!(
                "SELECT event, count() AS n, count(DISTINCT person_id) AS users, max(timestamp) AS last \
                 FROM events WHERE event IN ({}) AND {window} \
                 GROUP BY event HAVING users >= {min} ORDER BY users DESC LIMIT {MAX_FINDINGS}",
                names.join(", ")
            ),
        ));
    }
    if on("rageclick") {
        out.push((
            "rageclick".into(),
            format!(
                "SELECT properties.$pathname AS path, count() AS n, count(DISTINCT person_id) AS users, \
                 max(timestamp) AS last \
                 FROM events WHERE event = '$rageclick' AND {window} AND path IS NOT NULL \
                 GROUP BY path HAVING users >= {min} ORDER BY users DESC LIMIT {MAX_FINDINGS}"
            ),
        ));
    }
    if on("web_vitals") {
        out.push((
            "web_vitals".into(),
            format!(
                "SELECT properties.$pathname AS path, count() AS n, \
                 quantile(0.75)(toFloat(properties.$web_vitals_LCP_value)) AS lcp, \
                 quantile(0.75)(toFloat(properties.$web_vitals_INP_value)) AS inp, \
                 quantile(0.75)(toFloat(properties.$web_vitals_CLS_value)) AS cls, \
                 max(timestamp) AS last \
                 FROM events WHERE event = '$web_vitals' AND {window} AND path IS NOT NULL \
                 GROUP BY path HAVING n >= {MIN_VITALS_SAMPLES} ORDER BY n DESC LIMIT 100"
            ),
        ));
    }
    out.into_iter()
        .map(|(sig, q)| {
            (
                sig,
                serde_json::json!({ "query": { "kind": "HogQLQuery", "query": q } }),
            )
        })
        .collect()
}

/// PostHog timestamps come back as `2026-11-20T10:00:00.123000Z` or with an
/// offset; both are RFC-3339 enough for a string comparison after parsing.
fn ts(v: &Value) -> Option<String> {
    let t = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339())
}

/// The response of one [`posthog_queries`] entry → findings. Rows are
/// positional, in the column order the query selects.
pub fn posthog_findings(signal: &str, json: &Value, cfg: &PostHogSourceConfig) -> Vec<Finding> {
    let rows = json
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let base = cfg.host.trim_end_matches('/');
    let project = &cfg.project_id;
    let col = |r: &Value, i: usize| r.get(i).cloned().unwrap_or(Value::Null);
    let text = |v: &Value| -> String {
        match v {
            Value::String(t) => t.clone(),
            Value::Null => String::new(),
            // `$exception_types` is a JSON array, sometimes already a string.
            Value::Array(a) => a
                .first()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| x.to_string())
                })
                .unwrap_or_default(),
            other => other.to_string(),
        }
    };
    let first_of_json_list = |v: &Value| -> String {
        let t = text(v);
        // A string holding a JSON array: `["TypeError"]`.
        serde_json::from_str::<Vec<String>>(&t)
            .ok()
            .and_then(|l| l.into_iter().next())
            .unwrap_or(t)
    };
    let mut out = Vec::new();
    for r in rows {
        let f = match signal {
            "exception" => {
                let id = text(&col(&r, 0));
                if id.is_empty() {
                    continue;
                }
                let kind = first_of_json_list(&col(&r, 1));
                let msg = first_of_json_list(&col(&r, 2));
                let label = match (kind.is_empty(), msg.is_empty()) {
                    (false, false) => format!("{kind}: {msg}"),
                    (false, true) => kind,
                    (true, false) => msg,
                    (true, true) => format!("exception {id}"),
                };
                Finding {
                    source: "posthog".into(),
                    fingerprint: format!("posthog:exception:{id}"),
                    title: format!("Fix {}", one_line(&label, 100)),
                    body: format!(
                        "- PostHog error tracking: {base}/project/{project}/error_tracking/{id}\n\
                         - Events: {}, users: {}\n",
                        num(&col(&r, 3)),
                        num(&col(&r, 4))
                    ),
                    last_seen: ts(&col(&r, 5)),
                }
            }
            "failure_events" => {
                let event = text(&col(&r, 0));
                Finding {
                    source: "posthog".into(),
                    fingerprint: format!("posthog:event:{event}"),
                    title: format!("Investigate failures: `{}`", one_line(&event, 80)),
                    body: format!(
                        "- PostHog event `{event}` in the last {} days\n- Events: {}, users: {}\n",
                        cfg.window_days,
                        num(&col(&r, 1)),
                        num(&col(&r, 2))
                    ),
                    last_seen: ts(&col(&r, 3)),
                }
            }
            "rageclick" => {
                let path = text(&col(&r, 0));
                Finding {
                    source: "posthog".into(),
                    fingerprint: format!("posthog:rage:{path}"),
                    title: format!("Fix rage clicks on {}", one_line(&path, 80)),
                    body: format!(
                        "- PostHog `$rageclick` on `{path}` in the last {} days\n- Events: {}, users: {}\n\
                         - Find the element in session replays for this page.\n",
                        cfg.window_days,
                        num(&col(&r, 1)),
                        num(&col(&r, 2))
                    ),
                    last_seen: ts(&col(&r, 3)),
                }
            }
            "web_vitals" => {
                let path = text(&col(&r, 0));
                let (lcp, inp, cls) = (num(&col(&r, 2)), num(&col(&r, 3)), num(&col(&r, 4)));
                let mut bad = Vec::new();
                if lcp > LCP_POOR_MS {
                    bad.push(format!("LCP p75 {:.1}s (poor > 4s)", lcp / 1000.0));
                }
                if inp > INP_POOR_MS {
                    bad.push(format!("INP p75 {inp:.0}ms (poor > 500ms)"));
                }
                if cls > CLS_POOR {
                    bad.push(format!("CLS p75 {cls:.2} (poor > 0.25)"));
                }
                if bad.is_empty() {
                    continue;
                }
                Finding {
                    source: "posthog".into(),
                    fingerprint: format!("posthog:vitals:{path}"),
                    title: format!("Speed up {}", one_line(&path, 80)),
                    body: format!(
                        "- PostHog `$web_vitals` on `{path}`, {} samples\n- {}\n",
                        num(&col(&r, 1)),
                        bad.join("\n- ")
                    ),
                    last_seen: ts(&col(&r, 5)),
                }
            }
            _ => continue,
        };
        out.push(f);
        if out.len() >= MAX_FINDINGS {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- custom

fn ptr(p: &str, default: &str) -> String {
    if p.is_empty() {
        default.to_string()
    } else {
        p.to_string()
    }
}

/// A custom source's JSON → findings, using its configured pointers.
pub fn custom_findings(cfg: &CustomSourceConfig, json: &Value) -> Vec<Finding> {
    let items = if cfg.items.is_empty() {
        Some(json)
    } else {
        json.pointer(&cfg.items)
    };
    let (id_p, title_p) = (ptr(&cfg.id, "/id"), ptr(&cfg.title, "/title"));
    let field = |it: &Value, p: &str| -> String {
        if p.is_empty() {
            return String::new();
        }
        match it.pointer(p) {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    };
    items
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let id = field(it, &id_p);
            let title = one_line(&field(it, &title_p), 100);
            if id.is_empty() || title.is_empty() {
                return None;
            }
            let mut body = String::new();
            let link = field(it, &cfg.link);
            if !link.is_empty() {
                body.push_str(&format!("- Link: {link}\n"));
            }
            let text = field(it, &cfg.body);
            if !text.is_empty() {
                body.push_str(&format!("\n{}\n", text.trim()));
            }
            Some(Finding {
                source: format!("custom:{}", cfg.name),
                fingerprint: format!("custom:{}:{id}", cfg.name),
                title,
                body,
                last_seen: None,
            })
        })
        .take(MAX_FINDINGS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sentry_maps_issues_and_skips_idless() {
        let j = json!([
            {"id": "4411", "shortId": "WEB-1", "title": "TypeError: x is undefined",
             "culprit": "checkout/cart.ts", "permalink": "https://s/4411",
             "count": "120", "userCount": 37, "firstSeen": "2026-11-01T00:00:00Z",
             "lastSeen": "2026-11-20T00:00:00Z"},
            {"title": "no id"}
        ]);
        let f = sentry_findings(&j);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fingerprint, "sentry:4411");
        assert_eq!(
            f[0].title,
            "Fix TypeError: x is undefined in checkout/cart.ts"
        );
        assert!(f[0].body.contains("Events: 120, users: 37"));
        assert_eq!(f[0].last_seen.as_deref(), Some("2026-11-20T00:00:00Z"));
    }

    #[test]
    fn sentry_url_shape() {
        let cfg = SentrySourceConfig {
            org: "acme".into(),
            project: "web".into(),
            base_url: "https://sentry.io/".into(),
            ..Default::default()
        };
        assert_eq!(
            sentry_url(&cfg),
            "https://sentry.io/api/0/projects/acme/web/issues/?query=is%3Aunresolved%20lastSeen%3A-14d&sort=freq&limit=20"
        );
    }

    fn ph() -> PostHogSourceConfig {
        PostHogSourceConfig {
            project_id: "7".into(),
            failure_events: vec!["checkout_failed".into(), "it's".into()],
            ..Default::default()
        }
    }

    #[test]
    fn posthog_queries_follow_signals_and_quote_names() {
        let q = posthog_queries(&ph());
        let names: Vec<&str> = q.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            names,
            ["exception", "failure_events", "rageclick", "web_vitals"]
        );
        let fail = q[1].1["query"]["query"].as_str().unwrap();
        assert!(fail.contains("'checkout_failed', 'it\\'s'"));

        let mut cfg = ph();
        cfg.signals = vec!["rageclick".into()];
        cfg.failure_events.clear();
        assert_eq!(posthog_queries(&cfg).len(), 1);
    }

    #[test]
    fn posthog_exception_rows() {
        let j = json!({"results": [
            ["abc", "[\"TypeError\"]", "[\"x is undefined\"]", 40, 9, "2026-11-20T10:00:00.123Z"],
            [null, null, null, 1, 1, null]
        ]});
        let f = posthog_findings("exception", &j, &ph());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fingerprint, "posthog:exception:abc");
        assert_eq!(f[0].title, "Fix TypeError: x is undefined");
        assert!(f[0]
            .last_seen
            .as_deref()
            .unwrap()
            .starts_with("2026-11-20T10:00:00"));
    }

    #[test]
    fn posthog_web_vitals_files_only_poor_pages() {
        let j = json!({"results": [
            ["/slow", 80, 5200.0, 120.0, 0.05, "2026-11-20T10:00:00Z"],
            ["/fast", 80, 1200.0, 100.0, 0.01, "2026-11-20T10:00:00Z"]
        ]});
        let f = posthog_findings("web_vitals", &j, &ph());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fingerprint, "posthog:vitals:/slow");
        assert!(f[0].body.contains("LCP p75 5.2s"));
    }

    #[test]
    fn custom_uses_pointers_and_defaults() {
        let cfg = CustomSourceConfig {
            name: "linear".into(),
            items: "/data/issues".into(),
            link: "/url".into(),
            ..Default::default()
        };
        let j = json!({"data": {"issues": [
            {"id": 12, "title": "Login hangs", "url": "https://l/12"},
            {"id": 13}
        ]}});
        let f = custom_findings(&cfg, &j);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].fingerprint, "custom:linear:12");
        assert_eq!(f[0].source, "custom:linear");
        assert!(f[0].body.contains("https://l/12"));
    }
}
