//! The notification wire format.
//!
//! A copy of `crates/wingman-config/src/inbox.rs`'s types, not a dependency on
//! them: taking `wingman-config` would pull `keyring` — and libsecret/D-Bus on
//! Linux — into a binary that only needs to read two JSONL files. The two
//! copies are pinned by `encoding_is_the_documented_shape`, which asserts the
//! same bytes here and there, so a rename breaks a build rather than this app
//! at runtime.
//!
//! Parsing is lenient in the same way: blank lines and anything unrecognised
//! are skipped, never fatal. A newer `wingman` must not be able to wedge an
//! older notifier.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const INBOX_FILE: &str = "notifications.jsonl";
pub const REPLIES_FILE: &str = "notification-replies.jsonl";
pub const ALIVE_FILE: &str = "notifier.alive";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub severity: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub run_dir: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub free_text: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Notification {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }

    /// Whether this still wants an answer at `now`.
    pub fn open_at(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|e| e > now)
    }

    /// Whether a restart should bring this card back.
    ///
    /// `info` never comes back: it is a thing that happened, and re-showing it
    /// hours later is noise. Everything else is something a person still owes
    /// an answer to, and losing it because the app restarted is the failure
    /// this whole channel exists to avoid.
    pub fn worth_replaying(&self) -> bool {
        self.severity != "info"
    }
}

impl Reply {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("reply serializes")
    }
}

pub fn inbox_path(dir: &Path) -> PathBuf {
    dir.join(INBOX_FILE)
}

pub fn replies_path(dir: &Path) -> PathBuf {
    dir.join(REPLIES_FILE)
}

pub fn alive_path(dir: &Path) -> PathBuf {
    dir.join(ALIVE_FILE)
}

/// Relocates the global directory. A copy of `wingman_config::HOME_ENV` —
/// this is a separate cargo workspace and cannot import it, the same split the
/// wire format above already lives with. `home_env_matches_the_config_crate`
/// pins the two together.
pub const HOME_ENV: &str = "WINGMAN_HOME";

/// `~/.wingman/`, or [`HOME_ENV`] when it names an absolute path.
///
/// The launcher (`wingman notify`) resolves the same variable before deciding
/// whether a popup is already running and before compacting the inbox. If this
/// did not honour it too, the two would read different files and the app would
/// sit watching an inbox nobody writes to.
///
/// A relative value is ignored rather than refused: `wingman notify` already
/// refuses it with a message, and a GUI with no console has nowhere to complain
/// — falling back to the real home is the safer of the two silent options.
pub fn global_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(HOME_ENV) {
        let path = PathBuf::from(raw);
        if !path.to_string_lossy().trim().is_empty() && path.is_absolute() {
            return Some(path);
        }
    }
    Some(directories::BaseDirs::new()?.home_dir().join(".wingman"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_is_the_documented_shape() {
        // The twin of the assertion in crates/wingman-config/src/inbox.rs. If
        // these two ever disagree, one of them fails to build and somebody
        // finds out at compile time instead of in a silent popup.
        let n = Notification {
            id: "7-9".into(),
            severity: "info".into(),
            title: "hi".into(),
            created_at: 100,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&n).unwrap(),
            r#"{"id":"7-9","severity":"info","title":"hi","body":"","project":null,"run_dir":null,"created_at":100,"expires_at":null,"actions":[],"free_text":false}"#
        );
        assert_eq!(
            Reply {
                id: "7-9".into(),
                action: Some("yes".into()),
                text: None,
            }
            .encode(),
            r#"{"id":"7-9","action":"yes","text":null}"#
        );
        assert_eq!(
            serde_json::to_string(&Action {
                id: "a".into(),
                label: "A".into(),
                control: None,
            })
            .unwrap(),
            r#"{"id":"a","label":"A"}"#
        );
    }

    #[test]
    fn parsing_is_lenient_about_lines_it_does_not_understand() {
        assert_eq!(Notification::parse("   "), None);
        assert_eq!(Notification::parse("not json"), None);
        assert_eq!(Notification::parse(r#"{"id":1}"#), None);
        // A key this build has never heard of must not be fatal.
        let n = Notification::parse(
            r#"{"id":"a","severity":"decision","title":"t","created_at":1,"from_the_future":true}"#,
        )
        .expect("unknown keys are ignored");
        assert_eq!(n.id, "a");
        assert!(n.worth_replaying());
    }

    #[test]
    fn info_cards_do_not_come_back_after_a_restart() {
        let info = Notification {
            severity: "info".into(),
            ..Default::default()
        };
        assert!(!info.worth_replaying());
        for sev in ["decision", "escalation", "progress"] {
            let n = Notification {
                severity: sev.into(),
                ..Default::default()
            };
            assert!(n.worth_replaying(), "{sev} must survive a restart");
        }
    }

    #[test]
    fn an_expired_card_is_closed() {
        let now = now_secs();
        let n = Notification {
            expires_at: Some(now - 1),
            ..Default::default()
        };
        assert!(!n.open_at(now));
        assert!(Notification {
            expires_at: Some(now + 60),
            ..Default::default()
        }
        .open_at(now));
        // No deadline means informational: it never goes stale on its own.
        assert!(Notification::default().open_at(now));
    }

    #[test]
    fn home_env_matches_the_config_crate() {
        // Read the real declaration rather than trusting a copied string: the
        // launcher resolves this variable and the app has to agree, or the two
        // read different inboxes. Same trick as the UI's tokens.test.ts.
        let src = include_str!("../../../crates/wingman-config/src/paths.rs");
        let declared = src
            .lines()
            .find_map(|l| l.trim().strip_prefix("pub const HOME_ENV: &str = "))
            .expect("wingman-config declares HOME_ENV");
        assert_eq!(declared.trim_end_matches(';').trim_matches('"'), HOME_ENV);
    }

    #[test]
    fn an_absolute_home_env_wins_and_anything_else_falls_back() {
        // `global_dir` reads the process environment, so drive the rule rather
        // than the function — an env var cannot be varied under parallel tests.
        let pick = |raw: &str| {
            let p = PathBuf::from(raw);
            (!p.to_string_lossy().trim().is_empty() && p.is_absolute()).then_some(p)
        };
        let abs = if cfg!(windows) { "C:/wm" } else { "/tmp/wm" };
        assert_eq!(pick(abs), Some(PathBuf::from(abs)));
        assert_eq!(pick(""), None, "empty means unset");
        assert_eq!(pick("  "), None, "blank means unset");
        assert_eq!(pick("rel/ative"), None, "relative falls back to the home");
    }


    #[test]
    fn the_default_build_serves_its_own_assets() {
        // `custom-protocol` is Tauri's dev/production switch. Without it the
        // binary loads `devUrl` instead of the embedded assets, so a build with
        // no Vite server listening shows ERR_CONNECTION_REFUSED in a window
        // that never mounts, never calls `resize`, and therefore never appears.
        // Nothing else here can catch that: every other test exercises the file
        // tailing rather than the webview.
        //
        // Reading the manifest rather than `cfg!` on purpose — the flag is
        // *meant* to be absent under `--no-default-features`, so the invariant
        // is what `default` declares, not how this test happens to be compiled.
        let manifest = include_str!("../Cargo.toml");
        let default = manifest
            .lines()
            .find_map(|l| l.trim().strip_prefix("default = "))
            .expect("a [features] default list");
        assert!(
            default.contains("custom-protocol"),
            "`custom-protocol` must stay in the default features, found: {default}"
        );
    }

}
