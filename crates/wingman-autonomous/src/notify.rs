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
