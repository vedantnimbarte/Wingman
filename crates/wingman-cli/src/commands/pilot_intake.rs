//! `wingman pilot intake <slack|email|voice>` — external intake transports.
//!
//! The pilot daemon already ingests `*.md` request files from its `intake_dir`
//! (with per-author trust). These adapters *produce* those files from external
//! channels, so goals can arrive from Slack, email, or voice — not just the
//! CLI. Each adapter normalizes to the same intake file format:
//!
//! ```text
//! author: <name>
//! <request text>
//! ```
//!
//! The parsing/normalization is pure and unit-tested; the network/IO front ends
//! (Slack HTTP server, IMAP/maildir, STT transcript) wrap it.

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

/// `wingman pilot intake voice <transcript-file>` — ingest an STT transcript
/// (whatever tool produced it) as an intake request. The genuine "live mic"
/// front end still needs audio hardware + a local STT model, but any STT that
/// writes a transcript file feeds pilot through this.
pub async fn voice(transcript: String, author: Option<String>) -> Result<ExitCode> {
    let text = std::fs::read_to_string(&transcript)
        .with_context(|| format!("read transcript {transcript}"))?;
    let dir = intake_dir()?;
    let path = write_intake(&dir, author.as_deref(), &text)?;
    println!("voice intake → {}", path.display());
    Ok(ExitCode::SUCCESS)
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
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    eprintln!(
        "wingman pilot intake slack: listening on {addr}, writing to {}",
        dir.display()
    );

    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                continue;
            }
        };
        let dir = dir.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read until we have headers + full body (bounded).
            loop {
                match sock.read(&mut tmp).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() > 4 {
                            // Heuristic: got headers; assume small JSON bodies
                            // arrive in the same read burst.
                            break;
                        }
                        if buf.len() > 1_000_000 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf);
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
            let reply = handle_slack_body(&dir, body);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
    }
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
    let global = wingman_config::global_config_path()?;
    let project = wingman_config::ProjectPaths::discover(&std::env::current_dir()?);
    let project_file = project
        .config_file
        .exists()
        .then_some(project.config_file.clone());
    let cfg = wingman_config::Config::load(Some(&global), project_file.as_deref())?;
    Ok(project.root.join(&cfg.pilot.daemon.intake_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

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
