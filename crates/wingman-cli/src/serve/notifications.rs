//! `/v1/notifications` — the same actionable cards the desktop popup shows.
//!
//! The panel had grown its own notification path: a browser `Notification`
//! raised from the `/v1/events` stream, with its own idea of what was worth
//! interrupting someone for. That left two systems answering the same question
//! from different sources — the popup routed by `[pilot.notifications]` off the
//! inbox, the panel off run-status transitions with a hard-coded filter — and
//! they disagreed. A question from `ask_user` reached the popup and never
//! reached the panel at all, because a question is not a run transition.
//!
//! So the panel reads the inbox too, and the browser-notification path is gone.
//! One severity config, one filter, one wire format, and the two surfaces
//! cannot drift. It also puts actionable cards somewhere the popup can never
//! reach: `serve` is what a phone talks to.
//!
//! Replies mirror `desktop/notifier/src/tail.rs`, which is the same split the
//! wire format already lives with — the notifier is a separate cargo workspace
//! (decision 0018) and cannot call into this one.

use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use tokio::net::TcpStream;
use wingman_config::inbox;

use super::http::{self, Request};

/// Open cards, oldest first — exactly what the popup would draw.
pub async fn list(sock: &mut TcpStream) -> std::io::Result<()> {
    let dir = match wingman_config::global_dir() {
        Ok(d) => d,
        Err(e) => {
            return http::write_err(sock, 500, &format!("resolving the global dir: {e}")).await
        }
    };
    let open = inbox::read_open(&dir);
    http::write_json(sock, 200, &json!({ "notifications": open })).await
}

#[derive(Debug, Default, Deserialize)]
struct ReplyBody {
    /// The id of the button pressed, if any.
    #[serde(default)]
    action: Option<String>,
    /// The free-text box, if it was used.
    #[serde(default)]
    text: Option<String>,
}

/// Whether `p` is really a pilot run directory.
///
/// `run_dir` arrives inside a file, and acting on it means appending
/// caller-chosen JSON to a caller-chosen path. The inbox is 0600 and this route
/// is authenticated, so this is defence in depth rather than the only guard —
/// but it is five lines, and it is the rule the notifier already applies.
fn is_run_dir(p: &Path) -> bool {
    p.parent().is_some_and(|d| d.ends_with("autonomous"))
        && p.parent()
            .and_then(Path::parent)
            .is_some_and(|d| d.ends_with(".wingman"))
        && p.join("tasks.jsonl").is_file()
}

/// Answer one card. `action` is the button, `text` the box; both absent is a
/// dismissal, which still records a reply so the card does not come back.
pub async fn reply(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let dir = match wingman_config::global_dir() {
        Ok(d) => d,
        Err(e) => {
            return http::write_err(sock, 500, &format!("resolving the global dir: {e}")).await
        }
    };
    let body: ReplyBody = match req.json::<Option<ReplyBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };

    // Answering something already answered, expired, or never real is a 404
    // rather than a silent success: the panel should stop showing a card it
    // could not act on, and only the honest status tells it which.
    let Some(card) = inbox::read_open(&dir).into_iter().find(|n| n.id == id) else {
        return http::write_err(sock, 404, "no open notification with that id").await;
    };

    // A button carrying a control command routes to the run and stops there — a
    // run does not read the replies file, and writing both would leave a second
    // record of a decision already taken.
    let control = body
        .action
        .as_deref()
        .and_then(|a| card.actions.iter().find(|x| x.id == a))
        .and_then(|a| a.control.clone());

    let Some(raw) = control else {
        let r = inbox::Reply {
            id: id.to_string(),
            action: body.action,
            text: body.text,
        };
        return match inbox::append_reply_to(&dir, &r) {
            Ok(()) => http::write_json(sock, 202, &json!({ "answered": id, "via": "reply" })).await,
            Err(e) => http::write_err(sock, 500, &format!("writing reply: {e}")).await,
        };
    };

    let Some(run_dir) = card.run_dir.as_deref().map(Path::new) else {
        return http::write_err(
            sock,
            409,
            "that action needs a run, and this card names none",
        )
        .await;
    };
    if !is_run_dir(run_dir) {
        return http::write_err(sock, 409, "that card does not name a pilot run directory").await;
    }
    let cmd: wingman_autonomous::control::ControlCommand = match serde_json::from_value(raw) {
        Ok(c) => c,
        Err(e) => {
            return http::write_err(sock, 500, &format!("card carries a bad command: {e}")).await
        }
    };
    match wingman_autonomous::control::append(run_dir, &cmd) {
        Ok(()) => http::write_json(sock, 202, &json!({ "answered": id, "via": "control" })).await,
        Err(e) => http::write_err(sock, 500, &format!("writing control command: {e}")).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_directory_is_recognised_only_in_the_right_shape() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join(".wingman").join("autonomous").join("r1");
        std::fs::create_dir_all(&run).unwrap();
        // No tasks.jsonl yet: a directory no run has actually started in.
        assert!(!is_run_dir(&run));
        std::fs::write(run.join("tasks.jsonl"), "").unwrap();
        assert!(is_run_dir(&run));
    }

    #[test]
    fn a_plausible_looking_path_outside_a_run_is_refused() {
        let d = tempfile::tempdir().unwrap();
        // Right leaf name, wrong ancestry — the shape a chosen `run_dir` takes.
        let fake = d.path().join("autonomous").join("r1");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(fake.join("tasks.jsonl"), "").unwrap();
        assert!(!is_run_dir(&fake));
    }
}
