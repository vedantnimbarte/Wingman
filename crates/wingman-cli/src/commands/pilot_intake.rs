//! `wingman pilot intake <slack|email>` — external intake transports.
//!
//! The pilot daemon already ingests `*.md` request files from its `intake_dir`
//! (with per-author trust). These adapters *produce* those files from external
//! channels, so goals can arrive from Slack or email — not just the
//! CLI. Each adapter normalizes to the same intake file format:
//!
//! ```text
//! author: <name>
//! <request text>
//! ```
//!
//! The parsing/normalization is pure and unit-tested; the network/IO front ends
//! (Slack HTTP server, IMAP/maildir) wrap it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Write an intake `*.md` file from an author + text. Returns the path.
pub fn write_intake(dir: &Path, author: Option<&str>, text: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).ok();
    // Deterministic-ish unique name from a content hash + pid (no clock dep).
    let mut h: u64 = 1469598103934665603;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    let name = format!("intake-{}-{:x}.md", std::process::id(), h);
    let path = dir.join(name);
    let body = match author {
        Some(a) => format!("author: {a}\n{}\n", text.trim()),
        None => format!("{}\n", text.trim()),
    };
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Extract `(author, text)` from a Slack Events API payload. Handles the
/// `url_verification` handshake (returns None; the caller answers the
/// challenge) and `event_callback` message events. Ignores bot messages.
pub fn slack_event_to_intake(payload: &serde_json::Value) -> Option<(Option<String>, String)> {
    let ty = payload.get("type").and_then(|v| v.as_str())?;
    if ty != "event_callback" {
        return None;
    }
    let event = payload.get("event")?;
    if event.get("bot_id").is_some() {
        return None; // don't loop on our own / other bots' messages
    }
    let text = event
        .get("text")
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let user = event
        .get("user")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some((user, text))
}

/// Extract `(author, text)` from a raw RFC822 email (`.eml`): the `From:`
/// header becomes the author and the plain body becomes the text. Minimal
/// header parse (good enough for procmail/sieve-delivered mail).
pub fn eml_to_intake(raw: &str) -> Option<(Option<String>, String)> {
    // Split headers from body at the first blank line.
    let (headers, body) = raw
        .split_once("\n\n")
        .or_else(|| raw.split_once("\r\n\r\n"))?;
    let mut from = None;
    for line in headers.lines() {
        if let Some(v) = line
            .strip_prefix("From:")
            .or_else(|| line.strip_prefix("from:"))
        {
            // Prefer the address inside <...> when present.
            let v = v.trim();
            from = Some(
                v.split_once('<')
                    .and_then(|(_, r)| r.split_once('>').map(|(a, _)| a.to_string()))
                    .unwrap_or_else(|| v.to_string()),
            );
            break;
        }
    }
    let text = body.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some((from, text))
}

/// `wingman pilot intake email <maildir>` — convert every `.eml` in a directory
/// (delivered by the user's mail system) into an intake file, then delete it.
pub async fn email(maildir: String) -> Result<ExitCode> {
    let dir = intake_dir()?;
    let src = PathBuf::from(&maildir);
    let mut n = 0usize;
    if let Ok(rd) = std::fs::read_dir(&src) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("eml") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            if let Some((author, text)) = eml_to_intake(&raw) {
                write_intake(&dir, author.as_deref(), &text)?;
                let _ = std::fs::remove_file(&p);
                n += 1;
            }
        }
    }
    println!("email intake: ingested {n} message(s) → {}", dir.display());
    Ok(ExitCode::SUCCESS)
}

/// `wingman pilot intake slack --addr <ip:port>` — run a minimal HTTP server
/// receiving Slack Events API POSTs and writing each message as an intake file.
/// Answers the `url_verification` challenge. Point your Slack app's Event
/// Subscriptions Request URL at this server (behind your own TLS/ingress).
pub async fn slack(addr: String) -> Result<ExitCode> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = intake_dir()?;

    // A Slack signing secret is mandatory. This listener writes intake files
    // whose `author` field can promote a goal to TrustLevel::Trusted, which is
    // what permits unattended AutoRun — so an unauthenticated request here is
    // remote task execution against the repository. Refuse to start rather
    // than run an open door.
    let cfg = load_intake_config()?;
    let secret = cfg
        .pilot
        .daemon
        .slack_signing_secret
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to start: [pilot.daemon].slack_signing_secret is not set.\n\
                 Slack request signatures cannot be verified without it, and an \
                 unauthenticated intake server lets anyone who can reach the port \
                 queue work as a trusted author.\n\
                 Set it to your Slack app signing secret (a ${{ENV_VAR}} placeholder works)."
            )
        })?;

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    eprintln!(
        "wingman pilot intake slack: listening on {addr}, writing to {}",
        dir.display()
    );
    eprintln!("request signatures verified (Slack v0); POST /slack/events only");

    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        let dir = dir.clone();
        let secret = secret.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            // Read headers, then exactly `Content-Length` bytes of body. The
            // old code broke as soon as it saw the header terminator and
            // assumed the body had arrived in the same read — a split request
            // was silently dropped, and once signatures are checked a partial
            // body would fail verification for the wrong reason.
            loop {
                if let Some((body_start, len)) =
                    wingman_autonomous::webhook::header_boundary_and_len(&buf)
                {
                    if buf.len() >= body_start.saturating_add(len) {
                        break;
                    }
                }
                if buf.len() >= MAX_SLACK_REQUEST_BYTES {
                    break;
                }
                match sock.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = MAX_SLACK_REQUEST_BYTES.saturating_sub(buf.len());
                        buf.extend_from_slice(&tmp[..n.min(room)]);
                    }
                    Err(_) => break,
                }
            }

            let (status, reply) = handle_slack_request(&dir, &secret, &buf, now_unix());
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
    }
}

/// Cap on a single Slack request, headers included.
const MAX_SLACK_REQUEST_BYTES: usize = 1024 * 1024;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Case-insensitively read one header value out of a raw request.
fn header_value(headers: &str, name: &str) -> String {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// Route, authenticate, and handle one raw Slack request.
///
/// Returns `(status_line, body)`. Split out from the socket loop so the
/// security-relevant decisions are testable without binding a port.
fn handle_slack_request(dir: &Path, secret: &str, raw: &[u8], now: i64) -> (&'static str, String) {
    let Some((body_start, len)) = wingman_autonomous::webhook::header_boundary_and_len(raw) else {
        return ("400 Bad Request", String::new());
    };
    let head = String::from_utf8_lossy(&raw[..body_start]);
    let end = body_start.saturating_add(len).min(raw.len());
    let body = &raw[body_start..end];

    // Only the Slack events endpoint, and only POST. The previous version
    // never parsed the request line at all, so any method on any path was
    // accepted — and the url_verification branch echoed attacker-chosen bytes
    // back in a 200.
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("POST") || !path.starts_with("/slack/events") {
        return ("404 Not Found", String::new());
    }

    let ts = header_value(&head, "X-Slack-Request-Timestamp");
    let sig = header_value(&head, "X-Slack-Signature");
    if !wingman_autonomous::intake::slack_signature_valid(secret, &ts, body, &sig, now) {
        tracing::warn!(
            target: "pilot::intake",
            "rejected Slack request: signature invalid, stale, or missing"
        );
        return ("401 Unauthorized", String::new());
    }

    let reply = handle_slack_body(dir, &String::from_utf8_lossy(body));
    ("200 OK", reply)
}

/// Handle a Slack request body: answer url_verification, or write an intake
/// file for a message event. Returns the HTTP response body.
fn handle_slack_body(dir: &Path, body: &str) -> String {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return String::new();
    };
    if json.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        return json
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    if let Some((author, text)) = slack_event_to_intake(&json) {
        let _ = write_intake(dir, author.as_deref(), &text);
    }
    "ok".to_string()
}

fn intake_dir() -> Result<PathBuf> {
    let project = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let cfg = load_intake_config()?;
    Ok(project.root.join(&cfg.pilot.daemon.intake_dir))
}

/// The merged config, for the intake commands.
fn load_intake_config() -> Result<wingman_config::Config> {
    let global = wingman_config::global_config_path()?;
    let project = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project
        .config_file
        .exists()
        .then_some(project.config_file.clone());
    Ok(wingman_config::Config::load(
        Some(&global),
        project_file.as_deref(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw HTTP request with the given method/path/headers/body.
    fn raw_req(method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut s = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        s.push_str(body);
        s.into_bytes()
    }

    fn slack_sig(secret: &str, ts: &str, body: &str) -> String {
        let mut base = Vec::new();
        base.extend_from_slice(b"v0:");
        base.extend_from_slice(ts.as_bytes());
        base.push(b':');
        base.extend_from_slice(body.as_bytes());
        format!(
            "v0={}",
            wingman_autonomous::webhook::to_hex(&wingman_autonomous::webhook::hmac_sha256(
                secret.as_bytes(),
                &base
            ))
        )
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wm-slack-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    /// The whole point: an unsigned request must not be able to queue work.
    #[test]
    fn unsigned_slack_request_is_rejected_and_writes_nothing() {
        let dir = tmpdir("unsigned");
        let body = r#"{"type":"event_callback","event":{"type":"message","user":"vedant","text":"do a thing"}}"#;
        let raw = raw_req("POST", "/slack/events", &[], body);

        let (status, _) = handle_slack_request(&dir, "shh", &raw, 1_700_000_000);
        assert_eq!(status, "401 Unauthorized");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "no intake file may be written for an unauthenticated request"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forged_signature_is_rejected() {
        let dir = tmpdir("forged");
        let body =
            r#"{"type":"event_callback","event":{"type":"message","user":"vedant","text":"x"}}"#;
        let ts = "1700000000";
        // Signed with the wrong secret.
        let raw = raw_req(
            "POST",
            "/slack/events",
            &[
                ("X-Slack-Request-Timestamp", ts),
                ("X-Slack-Signature", &slack_sig("wrong", ts, body)),
            ],
            body,
        );
        let (status, _) = handle_slack_request(&dir, "shh", &raw, 1_700_000_010);
        assert_eq!(status, "401 Unauthorized");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn properly_signed_request_is_accepted() {
        let dir = tmpdir("signed");
        let body = r#"{"type":"event_callback","event":{"type":"message","user":"vedant","text":"fix the flaky test"}}"#;
        let ts = "1700000000";
        let raw = raw_req(
            "POST",
            "/slack/events",
            &[
                ("X-Slack-Request-Timestamp", ts),
                ("X-Slack-Signature", &slack_sig("shh", ts, body)),
            ],
            body,
        );
        let (status, reply) = handle_slack_request(&dir, "shh", &raw, 1_700_000_010);
        assert_eq!(status, "200 OK");
        assert_eq!(reply, "ok");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "a signed message should produce exactly one intake file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Previously any method on any path was accepted, and url_verification
    /// echoed attacker-chosen bytes back in a 200.
    #[test]
    fn wrong_method_or_path_is_not_found() {
        let dir = tmpdir("routing");
        let body = r#"{"type":"url_verification","challenge":"reflect-me"}"#;

        let (status, reply) =
            handle_slack_request(&dir, "shh", &raw_req("GET", "/slack/events", &[], body), 0);
        assert_eq!(status, "404 Not Found");
        assert!(reply.is_empty());

        let (status, reply) =
            handle_slack_request(&dir, "shh", &raw_req("POST", "/anything", &[], body), 0);
        assert_eq!(status, "404 Not Found");
        assert!(reply.is_empty(), "must not reflect attacker-chosen bytes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A body split across TCP reads must still verify: the reader now honours
    /// Content-Length instead of assuming one burst.
    #[test]
    fn body_is_taken_from_content_length_not_the_first_read() {
        let dir = tmpdir("contentlen");
        let body = r#"{"type":"event_callback","event":{"type":"message","user":"vedant","text":"hello"}}"#;
        let ts = "1700000000";
        let mut raw = raw_req(
            "POST",
            "/slack/events",
            &[
                ("X-Slack-Request-Timestamp", ts),
                ("X-Slack-Signature", &slack_sig("shh", ts, body)),
            ],
            body,
        );
        // Trailing bytes beyond Content-Length must not be folded into the
        // signed body.
        raw.extend_from_slice(b"GARBAGE");
        let (status, _) = handle_slack_request(&dir, "shh", &raw, 1_700_000_010);
        assert_eq!(status, "200 OK");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slack_message_event_extracts_user_and_text() {
        let payload = serde_json::json!({
            "type": "event_callback",
            "event": { "type": "message", "user": "U123", "text": "  fix the flaky test  " }
        });
        let (author, text) = slack_event_to_intake(&payload).unwrap();
        assert_eq!(author.as_deref(), Some("U123"));
        assert_eq!(text, "fix the flaky test");
    }

    #[test]
    fn slack_ignores_bot_and_verification() {
        let bot = serde_json::json!({ "type": "event_callback", "event": { "bot_id": "B1", "text": "hi" } });
        assert!(slack_event_to_intake(&bot).is_none());
        let verify = serde_json::json!({ "type": "url_verification", "challenge": "abc" });
        assert!(slack_event_to_intake(&verify).is_none());
    }

    #[test]
    fn slack_body_answers_challenge() {
        let dir = std::env::temp_dir();
        let body = r#"{"type":"url_verification","challenge":"xyz123"}"#;
        assert_eq!(handle_slack_body(&dir, body), "xyz123");
    }

    #[test]
    fn eml_extracts_from_and_body() {
        let raw = "From: Vedant <v@example.com>\r\nSubject: hi\r\n\r\nAdd a --version flag\r\n";
        let (author, text) = eml_to_intake(raw).unwrap();
        assert_eq!(author.as_deref(), Some("v@example.com"));
        assert_eq!(text, "Add a --version flag");
    }

    #[test]
    fn write_intake_uses_author_convention() {
        let dir = std::env::temp_dir().join(format!("wm-intake-{}", std::process::id()));
        let p = write_intake(&dir, Some("alice"), "do the thing").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("author: alice\n"));
        assert!(body.contains("do the thing"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
