//! Reading the inbox and writing answers back.
//!
//! The reader is a byte-offset tail, copied in shape from
//! `crates/wingman-autonomous/src/control.rs`'s `ControlReader` — including the
//! rule that a file which shrank is re-read from the top rather than silently
//! skipped.
//!
//! Answers go to one of two places. A button carrying a `control` command is
//! appended verbatim to `<run_dir>/control.jsonl`, which is the file a live
//! pilot run is already tailing; that is why approving a plan from here needs
//! no new machinery on the run's side. Everything else is a `Reply` on
//! `notification-replies.jsonl`, which is what `ask_user` waits on.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::wire::{self, Notification, Reply};

/// A restart brings back unanswered cards, but only recent ones: a laptop that
/// was shut for a week should not open onto last Tuesday's questions.
const REPLAY_WINDOW_SECS: u64 = 12 * 60 * 60;

/// Tails `notifications.jsonl` by byte offset.
pub struct Reader {
    path: PathBuf,
    offset: u64,
}

impl Reader {
    /// Start at the file's current end.
    ///
    /// Everything already written is history as far as the live stream is
    /// concerned — launching the app must not dump every card ever raised on
    /// the screen. What genuinely still needs an answer comes back through
    /// [`replay`] instead, which is selective about it.
    pub fn at_end(dir: &Path) -> Self {
        let path = wire::inbox_path(dir);
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, offset }
    }

    /// Notifications appended since the last poll.
    pub fn poll(&mut self) -> Vec<Notification> {
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
            .filter_map(Notification::parse)
            .collect()
    }
}

/// Ids that already have a reply.
fn answered(dir: &Path) -> HashSet<String> {
    let Ok(bytes) = std::fs::read(wire::replies_path(dir)) else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(Reply::parse)
        .map(|r| r.id)
        .collect()
}

/// Cards a fresh start should put back on screen: still open, still unanswered,
/// recent, and the kind somebody owes an answer to.
pub fn replay(dir: &Path, now: u64) -> Vec<Notification> {
    let Ok(bytes) = std::fs::read(wire::inbox_path(dir)) else {
        return Vec::new();
    };
    let done = answered(dir);
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(Notification::parse)
        .filter(|n| {
            n.worth_replaying()
                && n.open_at(now)
                && !done.contains(&n.id)
                && n.created_at.saturating_add(REPLAY_WINDOW_SECS) >= now
        })
        .collect()
}

/// Append one line with a single write syscall.
///
/// Not `writeln!`, which issues a separate write for the newline: several
/// wingman processes append to these files at once, and a torn line is a lost
/// notification. See the same note in `wingman-config`'s `inbox::append_line`.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
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

/// Whether `run_dir` is somewhere this app is willing to write a control
/// command.
///
/// It arrives as an absolute path inside a file, and acting on it means
/// appending caller-chosen JSON to a caller-chosen location. Two cheap
/// conditions close that: the path has to sit directly under a
/// `.wingman/autonomous/` directory, and it has to already contain the
/// `tasks.jsonl` a real run cannot start without. Neither costs a legitimate
/// run anything.
pub fn is_run_dir(run_dir: &Path) -> bool {
    let mut parents = run_dir.ancestors().skip(1);
    let autonomous = parents.next();
    let wingman = parents.next();
    let shaped = autonomous
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "autonomous")
        && wingman
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == ".wingman");
    shaped && run_dir.join("tasks.jsonl").is_file()
}

/// What answering a card did, for the log.
#[derive(Debug, PartialEq)]
pub enum Answered {
    /// A control command went to the run's own channel.
    Control,
    /// A reply went to the shared replies file.
    Reply,
}

/// Record the user's answer to `n`.
///
/// `action` is the id of the button pressed, if any; `text` the free-text box.
/// A button carrying a control command routes to the run and stops there — a
/// run does not read the replies file, and writing both would leave a second
/// record of a decision that was already taken.
pub fn answer(
    dir: &Path,
    n: &Notification,
    action: Option<&str>,
    text: Option<&str>,
) -> std::io::Result<Answered> {
    let control = action
        .and_then(|a| n.actions.iter().find(|x| x.id == a))
        .and_then(|a| a.control.as_ref());

    if let Some(cmd) = control {
        let run_dir = n.run_dir.as_deref().map(Path::new);
        match run_dir {
            Some(p) if is_run_dir(p) => {
                append_line(&p.join("control.jsonl"), &cmd.to_string())?;
                return Ok(Answered::Control);
            }
            _ => {
                return Err(std::io::Error::other(format!(
                    "refusing to write a control command to {:?}: not a pilot run directory",
                    n.run_dir
                )))
            }
        }
    }

    append_line(
        &wire::replies_path(dir),
        &Reply {
            id: n.id.clone(),
            action: action.map(str::to_string),
            text: text.map(str::to_string),
        }
        .encode(),
    )?;
    Ok(Answered::Reply)
}

/// Re-stamp the liveness marker. `ask_user` checks its age to decide whether
/// routing a question here is worth the wait.
pub fn touch_alive(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(wire::alive_path(dir), wire::now_secs().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Action;

    fn write(dir: &Path, n: &Notification) {
        append_line(&wire::inbox_path(dir), &serde_json::to_string(n).unwrap()).unwrap();
    }

    fn card(id: &str, severity: &str) -> Notification {
        Notification {
            id: id.into(),
            severity: severity.into(),
            title: "t".into(),
            created_at: wire::now_secs(),
            ..Default::default()
        }
    }

    #[test]
    fn tail_returns_only_newly_appended() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = Reader::at_end(dir.path());
        assert!(rx.poll().is_empty());

        write(dir.path(), &card("a", "info"));
        assert_eq!(rx.poll().len(), 1);
        assert!(rx.poll().is_empty(), "nothing new on the second poll");

        write(dir.path(), &card("b", "info"));
        assert_eq!(rx.poll()[0].id, "b");
    }

    #[test]
    fn tail_rereads_after_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = Reader::at_end(dir.path());
        write(dir.path(), &card("a-much-longer-id", "info"));
        assert_eq!(rx.poll().len(), 1);

        std::fs::write(wire::inbox_path(dir.path()), b"").unwrap();
        write(dir.path(), &card("b", "info"));
        assert_eq!(rx.poll()[0].id, "b");
    }

    #[test]
    fn tail_skips_lines_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = Reader::at_end(dir.path());
        append_line(&wire::inbox_path(dir.path()), "").unwrap();
        append_line(&wire::inbox_path(dir.path()), "{ not json").unwrap();
        write(dir.path(), &card("good", "info"));
        let got = rx.poll();
        assert_eq!(got.len(), 1, "one bad line must not eat the good one");
        assert_eq!(got[0].id, "good");
    }

    #[test]
    fn startup_replays_unanswered_actionable_only() {
        let dir = tempfile::tempdir().unwrap();
        let now = wire::now_secs();

        write(dir.path(), &card("question", "decision"));
        write(dir.path(), &card("chatter", "info"));
        write(dir.path(), &card("already-said", "decision"));
        write(
            dir.path(),
            &Notification {
                expires_at: Some(now - 1),
                ..card("too-late", "decision")
            },
        );
        write(
            dir.path(),
            &Notification {
                created_at: now - REPLAY_WINDOW_SECS - 1,
                ..card("last-tuesday", "decision")
            },
        );
        append_line(
            &wire::replies_path(dir.path()),
            &Reply {
                id: "already-said".into(),
                ..Default::default()
            }
            .encode(),
        )
        .unwrap();

        let back: Vec<String> = replay(dir.path(), now).into_iter().map(|n| n.id).collect();
        assert_eq!(back, vec!["question".to_string()]);
    }

    #[test]
    fn a_control_button_writes_to_the_run_not_the_replies_file() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(".wingman/autonomous/r1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("tasks.jsonl"), b"").unwrap();

        let n = Notification {
            run_dir: Some(run.display().to_string()),
            actions: vec![Action {
                id: "approve".into(),
                label: "Approve".into(),
                control: Some(serde_json::json!({ "cmd": "approve" })),
            }],
            ..card("gate", "decision")
        };

        assert_eq!(
            answer(dir.path(), &n, Some("approve"), None).unwrap(),
            Answered::Control
        );
        assert_eq!(
            std::fs::read_to_string(run.join("control.jsonl")).unwrap(),
            "{\"cmd\":\"approve\"}\n",
            "exactly the line `pilot approve` writes"
        );
        assert!(
            !wire::replies_path(dir.path()).exists(),
            "a decision already taken must not be recorded twice"
        );
    }

    #[test]
    fn a_plain_answer_goes_to_the_replies_file() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notification {
            free_text: true,
            actions: vec![Action {
                id: "sqlite".into(),
                label: "sqlite".into(),
                control: None,
            }],
            ..card("q", "decision")
        };

        assert_eq!(
            answer(dir.path(), &n, Some("sqlite"), Some("sqlite, WAL on")).unwrap(),
            Answered::Reply
        );
        let line = std::fs::read_to_string(wire::replies_path(dir.path())).unwrap();
        let r = Reply::parse(&line).unwrap();
        assert_eq!(r.id, "q");
        assert_eq!(r.action.as_deref(), Some("sqlite"));
        assert_eq!(r.text.as_deref(), Some("sqlite, WAL on"));
    }

    #[test]
    fn dismissing_a_card_records_it_so_a_restart_does_not_bring_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let n = card("seen", "escalation");
        write(dir.path(), &n);
        assert_eq!(replay(dir.path(), wire::now_secs()).len(), 1);

        answer(dir.path(), &n, None, None).unwrap();
        assert!(replay(dir.path(), wire::now_secs()).is_empty());
    }

    #[test]
    fn a_control_command_is_refused_outside_a_real_run_directory() {
        let dir = tempfile::tempdir().unwrap();
        let evil = dir.path().join("not-a-run");
        std::fs::create_dir_all(&evil).unwrap();

        let control = Some(serde_json::json!({ "cmd": "abort_run" }));
        let bad = |run_dir: Option<String>| Notification {
            run_dir,
            actions: vec![Action {
                id: "go".into(),
                label: "Go".into(),
                control: control.clone(),
            }],
            ..card("x", "decision")
        };

        // Right shape, but no `tasks.jsonl` — nothing ever ran here.
        let shaped = dir.path().join(".wingman/autonomous/ghost");
        std::fs::create_dir_all(&shaped).unwrap();
        assert!(answer(
            dir.path(),
            &bad(Some(shaped.display().to_string())),
            Some("go"),
            None
        )
        .is_err());

        // Wrong shape entirely.
        assert!(answer(
            dir.path(),
            &bad(Some(evil.display().to_string())),
            Some("go"),
            None
        )
        .is_err());

        // A control button with no run at all.
        assert!(answer(dir.path(), &bad(None), Some("go"), None).is_err());

        assert!(!evil.join("control.jsonl").exists());
    }
}
