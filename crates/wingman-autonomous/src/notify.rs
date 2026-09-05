//! R5 — notification routing & digesting.
//!
//! Autopilot + the daemon generate dozens of events a day. Without
//! routing, the user either drowns in pings or silences everything. Every
//! notification carries a [`NotificationSeverity`]; this module maps that
//! severity, via `[pilot.notifications]`, onto a [`RoutingDecision`]:
//! deliver now to a set of channels, batch into the daily digest, or
//! suppress.
//!
//! The [`Digest`] accumulator collects digested notifications so a cron
//! flush can emit them as one message.

use std::path::Path;

use wingman_config::PilotNotificationsConfig;

use crate::pr::CommandRunner;

/// Severity of a single notification (distinct from finding-[`crate::severity::Severity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSeverity {
    /// J15 trip, retry ladder exhausted, cost cap, R6 security hit.
    Escalation,
    /// Notify-only approval window, plan needs review.
    Decision,
    /// Task done, PR opened, run completed.
    Progress,
    /// Worker spawned, checkpoint saved, knowledge-graph updated.
    Info,
}

impl NotificationSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Escalation => "escalation",
            Self::Decision => "decision",
            Self::Progress => "progress",
            Self::Info => "info",
        }
    }
}

/// Where a notification goes.
#[derive(Debug, Clone, PartialEq)]
pub enum RoutingDecision {
    /// Deliver immediately to these channels (deduped, order-preserved).
    Immediate(Vec<String>),
    /// Add to the digest queue for the next scheduled flush.
    Digest,
    /// Drop silently.
    Suppress,
}

/// Interpret a single routing token (used for the `progress` / `info`
/// fields, which are one token rather than a channel list).
fn route_token(token: &str) -> RoutingDecision {
    match token.trim().to_ascii_lowercase().as_str() {
        "" | "suppress" | "none" | "off" => RoutingDecision::Suppress,
        "digest" => RoutingDecision::Digest,
        other => RoutingDecision::Immediate(
            other
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        ),
    }
}

fn route_channels(channels: &[String]) -> RoutingDecision {
    let cleaned: Vec<String> = channels
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("suppress"))
        .collect();
    if cleaned.is_empty() {
        RoutingDecision::Suppress
    } else {
        RoutingDecision::Immediate(cleaned)
    }
}

/// Where desktop cards are written, or `None` when the inbox is switched off
/// (or there is no home directory to write into).
///
/// One helper rather than `cfg.desktop_inbox && …` repeated at each emission
/// site: the switch and the path resolution belong together, and a site that
/// checks only one of them is the bug this shape prevents.
pub fn desktop_dir(config: &PilotNotificationsConfig) -> Option<std::path::PathBuf> {
    if !config.desktop_inbox {
        return None;
    }
    wingman_config::global_dir().ok()
}

/// The inbox directory a card of this severity should be written to, or `None`
/// when it should not be written at all.
///
/// Answers both halves of the question at once — is `desktop` a routed channel
/// for this severity, and is the inbox switched on — so an emission site cannot
/// honour one and forget the other. Call sites that go through
/// [`deliver_to_channels`] use [`desktop_dir`] instead; that function does its
/// own routing.
pub fn desktop_target(
    severity: NotificationSeverity,
    config: &PilotNotificationsConfig,
) -> Option<std::path::PathBuf> {
    match route(severity, config) {
        RoutingDecision::Immediate(channels) if channels.iter().any(|c| c == "desktop") => {
            desktop_dir(config)
        }
        _ => None,
    }
}

/// Route a notification of the given severity per config.
pub fn route(severity: NotificationSeverity, config: &PilotNotificationsConfig) -> RoutingDecision {
    match severity {
        NotificationSeverity::Escalation => route_channels(&config.escalation),
        NotificationSeverity::Decision => route_channels(&config.decision),
        NotificationSeverity::Progress => route_token(&config.progress),
        NotificationSeverity::Info => route_token(&config.info),
    }
}

/// One pending notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub severity: NotificationSeverity,
    pub title: String,
    pub body: String,
}

/// Accumulates digested notifications until a flush.
#[derive(Debug, Default)]
pub struct Digest {
    pending: Vec<Notification>,
}

impl Digest {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a notification, routing it. Returns the decision so the caller
    /// can deliver immediates itself. Digested ones are queued here.
    pub fn submit(
        &mut self,
        n: Notification,
        config: &PilotNotificationsConfig,
    ) -> RoutingDecision {
        let decision = route(n.severity, config);
        if decision == RoutingDecision::Digest {
            self.pending.push(n);
        }
        decision
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Render and clear the digest. Returns `None` when empty (skip the
    /// flush rather than sending an empty message).
    pub fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let mut out = format!("# Pilot digest ({} update(s))\n\n", self.pending.len());
        for n in &self.pending {
            out.push_str(&format!(
                "- [{}] {} — {}\n",
                n.severity.as_str(),
                n.title,
                n.body
            ));
        }
        self.pending.clear();
        Some(out)
    }
}

/// J3 channel sender shell: POST a notification body to a Slack/webhook
/// URL via `curl`. The runner abstraction makes it testable without a
/// network; the orchestrator wires the real [`crate::pr::SystemCommandRunner`].
pub fn send_webhook(runner: &dyn CommandRunner, url: &str, body: &str) -> Result<(), String> {
    let payload = serde_json::json!({ "text": body }).to_string();
    let out = runner
        .run(
            "curl",
            &[
                "-sS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &payload,
                url,
            ],
            Path::new("."),
        )
        .map_err(|e| format!("curl failed: {e}"))?;
    if out.success() {
        Ok(())
    } else {
        Err(format!("webhook POST failed: {}", out.stderr.trim()))
    }
}

/// Outcome of a delivery pass: which channels went out, which failed (with
/// the error), and which were routed but have no configured endpoint.
#[derive(Debug, Default, PartialEq)]
pub struct DeliveryReport {
    pub delivered: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub unconfigured: Vec<String>,
}

/// J3 delivery: for each routed `channel`, POST `body` to its configured
/// webhook (`webhooks[channel]`). Channels with no endpoint land in
/// `unconfigured`.
///
/// `terminal` is always skipped — the caller prints that one. `desktop` is
/// written to the notification inbox in the given directory when `desktop` is
/// `Some`, which is how `[pilot.notifications].desktop_inbox` reaches here;
/// passing `None` keeps the long-standing behaviour of skipping it so the
/// caller's `eprintln!` is the only delivery.
///
/// The inbox directory is a parameter rather than `global_dir()` read in here
/// so this stays testable without writing to the developer's real home.
pub fn deliver_to_channels(
    runner: &dyn CommandRunner,
    channels: &[String],
    webhooks: &std::collections::BTreeMap<String, String>,
    body: &str,
    desktop: Option<(&Path, &wingman_config::inbox::Notification)>,
) -> DeliveryReport {
    let mut report = DeliveryReport::default();
    for ch in channels {
        if ch == "terminal" {
            continue;
        }
        if ch == "desktop" {
            if let Some((dir, n)) = desktop {
                match wingman_config::inbox::append_to(dir, n) {
                    Ok(()) => report.delivered.push(ch.clone()),
                    Err(e) => report.failed.push((ch.clone(), e.to_string())),
                }
            }
            continue;
        }
        match webhooks.get(ch) {
            Some(url) if !url.trim().is_empty() => match send_webhook(runner, url, body) {
                Ok(()) => report.delivered.push(ch.clone()),
                Err(e) => report.failed.push((ch.clone(), e)),
            },
            _ => report.unconfigured.push(ch.clone()),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::{CommandOut, CommandRunner};
    use std::path::Path as StdPath;
    use std::sync::Mutex;

    fn cfg() -> PilotNotificationsConfig {
        PilotNotificationsConfig::default()
    }

    #[test]
    fn escalation_goes_to_all_channels_immediately() {
        let d = route(NotificationSeverity::Escalation, &cfg());
        match d {
            RoutingDecision::Immediate(ch) => {
                assert!(ch.contains(&"desktop".to_string()));
                assert!(ch.contains(&"slack".to_string()));
                assert!(ch.contains(&"email".to_string()));
            }
            _ => panic!("escalation must be immediate"),
        }
    }

    #[test]
    fn decision_is_immediate_subset() {
        assert_eq!(
            route(NotificationSeverity::Decision, &cfg()),
            RoutingDecision::Immediate(vec!["desktop".into(), "slack".into()])
        );
    }

    #[test]
    fn progress_defaults_to_digest() {
        assert_eq!(
            route(NotificationSeverity::Progress, &cfg()),
            RoutingDecision::Digest
        );
    }

    #[test]
    fn info_defaults_to_suppress() {
        assert_eq!(
            route(NotificationSeverity::Info, &cfg()),
            RoutingDecision::Suppress
        );
    }

    #[test]
    fn empty_channel_list_suppresses() {
        let mut c = cfg();
        c.escalation = vec![];
        assert_eq!(
            route(NotificationSeverity::Escalation, &c),
            RoutingDecision::Suppress
        );
    }

    #[test]
    fn token_can_name_explicit_channels() {
        let mut c = cfg();
        c.progress = "desktop, slack".into();
        assert_eq!(
            route(NotificationSeverity::Progress, &c),
            RoutingDecision::Immediate(vec!["desktop".into(), "slack".into()])
        );
    }

    struct RecordingCurl {
        calls: Mutex<Vec<Vec<String>>>,
    }
    impl CommandRunner for RecordingCurl {
        fn run(&self, program: &str, args: &[&str], _cwd: &StdPath) -> std::io::Result<CommandOut> {
            if program == "curl" {
                self.calls
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|s| s.to_string()).collect());
            }
            Ok(CommandOut {
                status: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn send_webhook_posts_json_payload() {
        let runner = RecordingCurl {
            calls: Mutex::new(Vec::new()),
        };
        send_webhook(&runner, "https://hooks.slack.com/x", "run done").unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let args = &calls[0];
        assert!(args.iter().any(|a| a == "https://hooks.slack.com/x"));
        assert!(args.iter().any(|a| a.contains("run done")));
    }

    #[test]
    fn deliver_routes_configured_and_reports_rest() {
        let runner = RecordingCurl {
            calls: Mutex::new(Vec::new()),
        };
        let mut webhooks = std::collections::BTreeMap::new();
        webhooks.insert("slack".to_string(), "https://hooks.slack.com/x".to_string());
        webhooks.insert("empty".to_string(), "  ".to_string()); // blank → unconfigured
        let channels = vec![
            "desktop".into(), // skipped: no inbox passed
            "slack".into(),   // delivered
            "email".into(),   // no entry → unconfigured
            "empty".into(),   // blank entry → unconfigured
        ];
        let report = deliver_to_channels(&runner, &channels, &webhooks, "run done", None);
        assert_eq!(report.delivered, vec!["slack".to_string()]);
        assert_eq!(
            report.unconfigured,
            vec!["email".to_string(), "empty".to_string()]
        );
        assert!(report.failed.is_empty());
        // Exactly one webhook POST (slack); desktop/email/empty didn't POST.
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_desktop_channel_writes_the_inbox_only_when_it_is_enabled() {
        use wingman_config::inbox;

        let runner = RecordingCurl {
            calls: Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let channels = vec!["desktop".to_string()];
        let webhooks = std::collections::BTreeMap::new();
        let n = inbox::Notification::now("escalation", "Run failed", "3 tasks did not finish");

        // Off (`desktop_inbox = false`): skipped entirely, as it has always
        // been — the caller's terminal print is the whole delivery.
        let off = deliver_to_channels(&runner, &channels, &webhooks, "b", None);
        assert_eq!(
            off,
            DeliveryReport::default(),
            "nothing routed, nothing said"
        );
        assert!(!inbox::inbox_path(dir.path()).exists());

        // On: one card, and reported as delivered rather than unconfigured —
        // `desktop` has no webhook and must not be blamed for lacking one.
        let on = deliver_to_channels(&runner, &channels, &webhooks, "b", Some((dir.path(), &n)));
        assert_eq!(on.delivered, vec!["desktop".to_string()]);
        assert!(on.unconfigured.is_empty());
        let open = inbox::read_open(dir.path());
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].title, "Run failed");

        // Either way this channel never POSTs.
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn digest_accumulates_and_flushes() {
        let c = cfg();
        let mut digest = Digest::new();
        // Progress → digest (queued); escalation → immediate (not queued).
        let d1 = digest.submit(
            Notification {
                severity: NotificationSeverity::Progress,
                title: "task done".into(),
                body: "t1 merged".into(),
            },
            &c,
        );
        assert_eq!(d1, RoutingDecision::Digest);
        let d2 = digest.submit(
            Notification {
                severity: NotificationSeverity::Escalation,
                title: "cost cap".into(),
                body: "halted".into(),
            },
            &c,
        );
        assert!(matches!(d2, RoutingDecision::Immediate(_)));
        assert_eq!(digest.pending_count(), 1);

        let flushed = digest.flush().unwrap();
        assert!(flushed.contains("task done"));
        assert!(!flushed.contains("cost cap")); // escalation wasn't digested
        assert!(digest.is_empty());
        // Second flush with nothing pending → None.
        assert!(digest.flush().is_none());
    }
}
