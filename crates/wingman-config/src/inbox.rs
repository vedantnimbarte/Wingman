//! The desktop notification inbox: two append-only JSONL files under
//! `~/.wingman/` that let any wingman process raise an *actionable* prompt and
//! collect the answer.
//!
//! A pilot run is a detached process, a worker is its child, the TUI is your
//! terminal, and `wingman serve` may not be running at all. There is no
//! in-process channel that reaches all of them, so this borrows the shape that
//! already works for live runs — `wingman_autonomous::control`, where any
//! process appends a JSON line and the interested one tails it by byte offset.
//!
//! ```text
//! ~/.wingman/notifications.jsonl        any process appends; the desktop app reads
//! ~/.wingman/notification-replies.jsonl the desktop app appends; the asker tails
//! ~/.wingman/notifier.alive             the desktop app touches this while resident
//! ```
//!
//! **Approvals do not travel on the reply file.** A notification carries the
//! run directory and an approve/veto button carries the literal
//! `ControlCommand` JSON, so the desktop app appends it straight to
//! `<run_dir>/control.jsonl` — the file the run is already tailing. That keeps
//! the approval gate a zero-line change, and means a new `ControlCommand`
//! becomes a new button without the app ever learning the vocabulary.
//!
//! Nothing loss-critical rides this channel: approvals go via `control.jsonl`,
//! and a dropped reply degrades to `ask_user`'s existing "proceed with your
//! best judgment" note.
//!
//! Why this lives in `wingman-config` rather than a crate of its own:
//! `wingman-tools` needs it for `ask_user` and does **not** depend on
//! `wingman-autonomous` — the dependency runs the other way, as that crate's
//! own doc records for `child_process`. This crate is the lowest node both can
//! see, and it already owns the `~/.wingman/` layout in `paths.rs`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Notifications file, inside the global dir.
pub const INBOX_FILE: &str = "notifications.jsonl";
/// Replies file, inside the global dir.
pub const REPLIES_FILE: &str = "notification-replies.jsonl";
/// Liveness marker the desktop app re-touches while it is running.
pub const ALIVE_FILE: &str = "notifier.alive";

/// How stale [`ALIVE_FILE`] may be before the app counts as gone. Generous
/// against the app's own ~10s touch interval, because the cost of guessing
/// "alive" wrongly is a caller waiting out its whole timeout for nobody.
const ALIVE_WINDOW: Duration = Duration::from_secs(30);

/// Notifications older than this are dropped by [`read_open`] regardless of
/// `expires_at`, so a machine that was asleep for a week does not wake to a
/// wall of stale cards.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// One thing worth interrupting someone for.
///
/// Field order is the wire order, and `encoding_is_the_documented_shape` pins
/// it: the desktop app carries its own copy of this struct across a workspace
/// boundary, so a rename has to break the build here rather than the app at
/// runtime.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Unique across processes. `<pid>-<nanos>` — no `rand` dependency for
    /// something only two local processes ever compare.
    pub id: String,
    /// `"escalation"` | `"decision"` | `"progress"` | `"info"`.
    ///
    /// A `String` and not an enum on purpose: the enum is
    /// `wingman_autonomous::notify::NotificationSeverity`, and this crate must
    /// not depend upward on that one. Producers pass its `as_str()`.
    pub severity: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Display name of the project this came from, when there is one.
    #[serde(default)]
    pub project: Option<String>,
    /// Absolute path of the pilot run directory. Present only for run-scoped
    /// notifications; it is what makes an [`Action::control`] button possible.
    #[serde(default)]
    pub run_dir: Option<String>,
    /// Unix epoch seconds.
    pub created_at: u64,
    /// When the asker stops listening. Past it the app must stop offering to
    /// answer — the reply would go nowhere. `None` means informational.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Buttons, left to right. Empty means a plain toast.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Render a free-text box alongside the buttons.
    #[serde(default)]
    pub free_text: bool,
}

/// One button on a notification.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    pub label: String,
    /// When present, the desktop app appends this value verbatim as one line to
    /// `<run_dir>/control.jsonl` *instead of* writing a reply. It is a
    /// serialised `wingman_autonomous::control::ControlCommand`, e.g.
    /// `{"cmd":"approve"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<serde_json::Value>,
}

/// The user's answer to a notification that did not carry a control command.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The [`Notification::id`] being answered.
    pub id: String,
    /// The [`Action::id`] clicked, if any.
    #[serde(default)]
    pub action: Option<String>,
    /// Free-text answer, if the box was used.
    #[serde(default)]
    pub text: Option<String>,
}

/// Seconds since the epoch. Saturates rather than panicking on a clock set
/// before 1970, which happens on boards with a dead RTC.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Notification {
    /// A notification stamped with a fresh id and the current time. Callers
    /// fill the rest with struct-update syntax:
    ///
    /// ```ignore
    /// Notification {
    ///     run_dir: Some(p),
    ///     actions: vec![approve, veto],
    ///     ..Notification::now("decision", title, body)
    /// }
    /// ```
    pub fn now(
        severity: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            id: format!("{}-{}", std::process::id(), nanos),
            severity: severity.into(),
            title: title.into(),
            body: body.into(),
            created_at: now_secs(),
            ..Self::default()
        }
    }

    /// Serialise to a single JSON line (no trailing newline).
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("notification serializes")
    }

    /// Parse one line, returning `None` for anything that isn't one. Lenient
    /// like `ControlCommand::parse`: a newer writer must never wedge an older
    /// reader.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }

    /// Whether this still wants an answer at `now`.
    fn open_at(&self, now: u64) -> bool {
        if self.created_at.saturating_add(MAX_AGE.as_secs()) < now {
            return false;
        }
        self.expires_at.is_none_or(|e| e > now)
    }
}

impl Reply {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("reply serializes")
    }

    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
    }

    /// The answer as `ask_user` wants to report it: the free text if the box
    /// was used, else the clicked action's id.
    pub fn answer(&self) -> Option<&str> {
        self.text
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .or(self.action.as_deref())
    }
}

/* ── Files ────────────────────────────────────────────────────────────── */

pub fn inbox_path(dir: &Path) -> PathBuf {
    dir.join(INBOX_FILE)
}

pub fn replies_path(dir: &Path) -> PathBuf {
    dir.join(REPLIES_FILE)
}

pub fn alive_path(dir: &Path) -> PathBuf {
    dir.join(ALIVE_FILE)
}

fn global() -> std::io::Result<PathBuf> {
    crate::global_dir().map_err(|e| std::io::Error::other(e.to_string()))
}

/// Append one line, with **one** write syscall.
///
/// Not `writeln!`: that goes through `write_fmt`, which issues a separate write
/// for the body and for the newline — `wingman_autonomous::store` already
/// tolerates a torn final line for exactly that reason. A per-run control file
/// gets away with it because one process appends at a time; this file is
/// written by a detached run, its workers, a TUI and a `serve` child at once.
///
/// With a single `write_all` the append is atomic: on POSIX, `O_APPEND` seeks
/// and writes atomically with respect to other writers, and Rust's
/// `append(true)` asks Win32 for `FILE_APPEND_DATA` without `FILE_WRITE_DATA`,
/// which is the documented atomic-append mode. (Not true over NFS, which has no
/// atomic append — not worth engineering around for a local notification file.)
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    // 0600 at creation rather than chmod after: a second local user must not be
    // able to inject a notification carrying an attacker-chosen `run_dir`, and
    // there must be no window where the file exists world-writable. Same
    // reasoning as `write_private`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    f.write_all(buf.as_bytes())
}

/// Append a notification to the inbox in `dir`.
pub fn append_to(dir: &Path, n: &Notification) -> std::io::Result<()> {
    append_line(&inbox_path(dir), &n.encode())
}

/// Append a notification to `~/.wingman/notifications.jsonl`.
pub fn append(n: &Notification) -> std::io::Result<()> {
    append_to(&global()?, n)
}

/// Append a reply to the replies file in `dir`.
pub fn append_reply_to(dir: &Path, r: &Reply) -> std::io::Result<()> {
    append_line(&replies_path(dir), &r.encode())
}

/// Append a reply to `~/.wingman/notification-replies.jsonl`.
pub fn append_reply(r: &Reply) -> std::io::Result<()> {
    append_reply_to(&global()?, r)
}

/// Ids that already have a reply.
fn answered(dir: &Path) -> HashSet<String> {
    let Ok(bytes) = std::fs::read(replies_path(dir)) else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(Reply::parse)
        .map(|r| r.id)
        .collect()
}

/// Notifications still worth showing: not answered, not expired, not older than
/// a day. Oldest first, matching file order.
///
/// Expiry happens on read rather than by trimming the file. Every trim scheme
/// races a concurrent appender — read-then-rename loses lines written in the
/// window, rotate-then-recreate loses whatever is still held on the old fd —
/// and there is nothing on this channel worth that.
///
// ponytail: both files grow forever. ~200 bytes a line and a few dozen lines on
// a heavy day, and readers are O(1) via byte offset, so this is a disk-space
// ceiling rather than a correctness or latency one. Add a size-triggered
// rewrite (temp-and-rename, as `store::snapshot` does) if one passes ~10 MB.
pub fn read_open(dir: &Path) -> Vec<Notification> {
    let Ok(bytes) = std::fs::read(inbox_path(dir)) else {
        return Vec::new();
    };
    let done = answered(dir);
    let now = now_secs();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(Notification::parse)
        .filter(|n| n.open_at(now) && !done.contains(&n.id))
        .collect()
}

/// Whether the desktop app is running, judged by how recently it touched
/// [`ALIVE_FILE`].
///
/// This is what keeps the `ask_user` route honest: with the feature enabled but
/// the app closed, a caller must fall through to its terminal prompt rather
/// than sit out a two-minute timeout nobody is going to answer.
pub fn notifier_alive_in(dir: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(alive_path(dir)) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    // A file stamped in the future (clock skew, a restored backup) counts as
    // fresh: `elapsed` errors rather than returning a negative duration.
    modified.elapsed().map(|d| d < ALIVE_WINDOW).unwrap_or(true)
}

/// [`notifier_alive_in`] against `~/.wingman/`.
pub fn notifier_alive() -> bool {
    global().map(|d| notifier_alive_in(&d)).unwrap_or(false)
}

/// Re-stamp [`ALIVE_FILE`]. The desktop app calls this on its poll loop; it
/// lives here so both sides agree on the file without the app guessing.
pub fn touch_alive_in(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(alive_path(dir), now_secs().to_string())
}

/* ── Tailing replies ──────────────────────────────────────────────────── */

/// Tails the replies file, remembering how far it has consumed so each reply is
/// seen exactly once across polls. Mirrors `ControlReader`, including the
/// shrink-means-reread rule.
#[derive(Debug)]
pub struct ReplyReader {
    path: PathBuf,
    offset: u64,
}

impl ReplyReader {
    /// Start at the file's **current** length.
    ///
    /// Deliberately not `ControlReader`'s start-at-zero: a run's control file is
    /// per-run and fresh, but this one is global and long-lived, so starting at
    /// zero would replay every reply ever written and let a stale answer satisfy
    /// a new question. It also keeps each poll O(new bytes) however large the
    /// file has grown.
    ///
    /// Construct this *before* appending the notification it waits on, or a
    /// reply that lands in between is skipped.
    pub fn at_end_in(dir: &Path) -> Self {
        let path = replies_path(dir);
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, offset }
    }

    /// [`Self::at_end_in`] against `~/.wingman/`.
    pub fn at_end() -> std::io::Result<Self> {
        Ok(Self::at_end_in(&global()?))
    }

    /// Replies appended since the last poll. A missing file yields nothing; a
    /// file that shrank is re-read from the top rather than silently skipped.
    ///
    /// Shrinkage is detected by length alone, exactly as `ControlReader` does,
    /// so a file truncated and regrown to the *same* byte count reads as "no
    /// change". Nothing truncates this file by design — the branch is there so a
    /// hand-deleted file cannot wedge the reader forever — and the next append
    /// moves the length past the stale offset anyway.
    pub fn poll(&mut self) -> Vec<Reply> {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return Vec::new();
        };
        let len = bytes.len() as u64;
        if len < self.offset {
            self.offset = 0;
        }
        if len == self.offset {
            return Vec::new();
        }
        let start = self.offset as usize;
        self.offset = len;
        String::from_utf8_lossy(&bytes[start..])
            .lines()
            .filter_map(Reply::parse)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn note(id: &str) -> Notification {
        Notification {
            id: id.into(),
            severity: "info".into(),
            title: "t".into(),
            created_at: now_secs(),
            ..Default::default()
        }
    }

    fn reply(id: &str) -> Reply {
        Reply {
            id: id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_through_json() {
        let n = Notification {
            id: "1-2".into(),
            severity: "decision".into(),
            title: "Plan awaiting approval".into(),
            body: "7 tasks".into(),
            project: Some("wingman".into()),
            run_dir: Some("/p/.wingman/autonomous/r1".into()),
            created_at: 1_757_068_923,
            expires_at: Some(1_757_070_723),
            actions: vec![Action {
                id: "approve".into(),
                label: "Approve".into(),
                control: Some(serde_json::json!({ "cmd": "approve" })),
            }],
            free_text: true,
        };
        assert_eq!(Notification::parse(&n.encode()), Some(n));

        let r = Reply {
            id: "1-2".into(),
            action: Some("approve".into()),
            text: Some("go".into()),
        };
        assert_eq!(Reply::parse(&r.encode()), Some(r));
    }

    #[test]
    fn encoding_is_the_documented_shape() {
        // The desktop app carries its own copy of these structs across a
        // workspace boundary and asserts the same strings. A rename must break
        // the build here, not the app at runtime.
        let n = Notification {
            id: "7-9".into(),
            severity: "info".into(),
            title: "hi".into(),
            created_at: 100,
            ..Default::default()
        };
        assert_eq!(
            n.encode(),
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
        // `control` is omitted when absent, so a plain button stays small.
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
    fn parse_skips_blank_and_garbage_lines() {
        assert_eq!(Notification::parse("   "), None);
        assert_eq!(Notification::parse("not json"), None);
        assert_eq!(Notification::parse(r#"{"id":1}"#), None); // wrong types
        assert_eq!(Reply::parse(""), None);
        assert_eq!(Reply::parse("{"), None);
        // A hand-written minimal line still parses, and unknown keys from a
        // newer writer are ignored rather than fatal.
        let n = Notification::parse(
            r#"  {"id":"a","severity":"info","title":"t","created_at":1,"future_key":3}  "#,
        )
        .expect("lenient parse");
        assert_eq!(n.id, "a");
    }

    #[test]
    fn reply_reader_returns_only_newly_appended() {
        let dir = tempdir().unwrap();
        let mut rx = ReplyReader::at_end_in(dir.path());
        assert!(rx.poll().is_empty());

        append_reply_to(dir.path(), &reply("a")).unwrap();
        assert_eq!(rx.poll().len(), 1);
        assert!(rx.poll().is_empty(), "second poll with no writes");

        append_reply_to(dir.path(), &reply("b")).unwrap();
        assert_eq!(rx.poll()[0].id, "b");
    }

    #[test]
    fn reply_reader_rereads_after_truncation() {
        let dir = tempdir().unwrap();
        let mut rx = ReplyReader::at_end_in(dir.path());
        append_reply_to(dir.path(), &reply("a-longer-id")).unwrap();
        assert_eq!(rx.poll().len(), 1);

        // The replacement line must be *shorter* than the offset for length
        // alone to reveal the truncation — see `poll`'s note. Nothing truncates
        // this file in production; the branch exists so a hand-deleted file
        // cannot wedge the reader forever.
        std::fs::write(replies_path(dir.path()), b"").unwrap();
        append_reply_to(dir.path(), &reply("b")).unwrap();
        assert_eq!(rx.poll()[0].id, "b");
    }

    #[test]
    fn reply_reader_at_end_ignores_prior_replies() {
        // The stale-answer guard. If someone "simplifies" `at_end_in` to start
        // at zero, a question inherits the answer to the previous one.
        let dir = tempdir().unwrap();
        append_reply_to(dir.path(), &reply("old")).unwrap();

        let mut rx = ReplyReader::at_end_in(dir.path());
        assert!(rx.poll().is_empty(), "prior replies must not be replayed");

        append_reply_to(dir.path(), &reply("new")).unwrap();
        assert_eq!(rx.poll()[0].id, "new");
    }

    #[test]
    fn append_is_atomic_under_concurrency() {
        // The reason `append_line` uses one `write_all` instead of `writeln!`.
        // With two syscalls per line, interleaved writers tear lines and this
        // fails.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::thread::scope(|s| {
            for t in 0..8 {
                let root = root.clone();
                s.spawn(move || {
                    for i in 0..50 {
                        append_to(&root, &note(&format!("{t}-{i}"))).unwrap();
                    }
                });
            }
        });
        let text = std::fs::read_to_string(inbox_path(&root)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 400, "one line per append");
        assert_eq!(
            lines.iter().filter_map(|l| Notification::parse(l)).count(),
            400,
            "every line intact"
        );
    }

    #[test]
    fn read_open_drops_expired_and_answered() {
        let dir = tempdir().unwrap();
        let now = now_secs();

        append_to(dir.path(), &note("keep")).unwrap();
        append_to(
            dir.path(),
            &Notification {
                expires_at: Some(now - 1),
                ..note("expired")
            },
        )
        .unwrap();
        append_to(dir.path(), &note("answered")).unwrap();
        append_to(
            dir.path(),
            &Notification {
                created_at: now - MAX_AGE.as_secs() - 1,
                ..note("ancient")
            },
        )
        .unwrap();
        append_reply_to(dir.path(), &reply("answered")).unwrap();

        let open: Vec<String> = read_open(dir.path()).into_iter().map(|n| n.id).collect();
        assert_eq!(open, vec!["keep".to_string()]);
    }

    #[test]
    fn notifier_alive_tracks_the_marker() {
        let dir = tempdir().unwrap();
        assert!(
            !notifier_alive_in(dir.path()),
            "no marker means not running"
        );
        touch_alive_in(dir.path()).unwrap();
        assert!(notifier_alive_in(dir.path()));
    }

    #[test]
    fn reply_answer_prefers_free_text_over_the_clicked_action() {
        let r = Reply {
            id: "a".into(),
            action: Some("sqlite".into()),
            text: Some("sqlite, WAL on".into()),
        };
        assert_eq!(r.answer(), Some("sqlite, WAL on"));

        let r = Reply {
            id: "a".into(),
            action: Some("sqlite".into()),
            text: Some("  ".into()),
        };
        assert_eq!(r.answer(), Some("sqlite"), "blank text falls back");
        assert_eq!(reply("a").answer(), None);
    }
}
