//! J3 inbound webhook receiver — a minimal, dependency-free HTTP/1.1
//! endpoint that turns `POST /goals` into an intake [`Goal`].
//!
//! Built on `std::net::TcpListener` so it pulls in no web framework and is
//! fully testable over a loopback socket. The daemon binds this on a
//! configured port; each accepted goal is handed to the caller's callback
//! (which enqueues it for the same auto/notify/gate path every other
//! channel uses).
//!
//! Scope: this is the local-HTTP intake adapter from the J3 table. Slack
//! and email adapters are thin transforms over the same
//! [`crate::intake::normalize`] pipeline once their transport delivers a
//! body; the HTTP receiver is the one that needs a socket, and it's here.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::intake::{normalize, Channel, Goal};

/// Split the body (everything after the first blank line) out of a raw
/// HTTP request. Returns `None` when there's no body section.
pub fn parse_http_body(raw: &str) -> Option<&str> {
    // Headers and body are separated by CRLFCRLF (tolerate bare LFLF).
    if let Some(idx) = raw.find("\r\n\r\n") {
        return Some(&raw[idx + 4..]);
    }
    if let Some(idx) = raw.find("\n\n") {
        return Some(&raw[idx + 2..]);
    }
    None
}

/// Extract `(text, author)` from a JSON body of the shape
/// `{"goal": "...", "author": "..."}` (also accepts `"text"` for `goal`).
pub fn extract_goal_fields(body: &str) -> Option<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let text = v
        .get("goal")
        .or_else(|| v.get("text"))
        .and_then(|t| t.as_str())?
        .to_string();
    if text.trim().is_empty() {
        return None;
    }
    let author = v
        .get("author")
        .and_then(|a| a.as_str())
        .map(|s| s.to_string());
    Some((text, author))
}

/// HMAC-SHA256 shared-secret authentication for the webhook receiver.
///
/// When `secret` is `Some`, every request must carry a valid
/// `X-Arccode-Signature: sha256=<hex>` header computed over the raw request
/// body; unsigned or wrongly-signed requests are rejected with `401`. Only an
/// authenticated request may have its body-claimed `author` honored for trust
/// classification — without a secret, authors are always treated as anonymous
/// (never `Trusted`), so an unauthenticated caller can't spoof an allowlisted
/// identity into an auto-run.
pub struct WebhookAuth<'a> {
    pub secret: Option<&'a [u8]>,
    pub trusted_authors: &'a [String],
}

impl<'a> WebhookAuth<'a> {
    /// An unauthenticated receiver (no shared secret): requests are accepted
    /// but their claimed authors never earn trust.
    pub fn anonymous(trusted_authors: &'a [String]) -> Self {
        Self {
            secret: None,
            trusted_authors,
        }
    }
}

/// Hard cap on how much of a request we'll buffer, so a client can't stream
/// forever and exhaust memory.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Read one HTTP request off `stream`, authenticate it (when a secret is
/// configured), parse a goal from its body, write a minimal response, and
/// return the normalised [`Goal`] — or `None` on an unauthorized / malformed
/// / empty request, after replying `401`/`400`.
pub fn handle_connection(
    stream: &mut TcpStream,
    auth: &WebhookAuth,
) -> std::io::Result<Option<Goal>> {
    let raw = read_request(stream)?;
    let (body_start, _len) = match header_boundary_and_len(&raw) {
        Some(v) => v,
        None => {
            write_response(stream, "400 Bad Request", "bad req")?;
            return Ok(None);
        }
    };
    let header_str = String::from_utf8_lossy(&raw[..body_start]);
    let body_bytes = &raw[body_start..];

    // Authenticate against the HMAC signature when a secret is configured.
    let authenticated = match auth.secret {
        Some(secret) => extract_signature(&header_str)
            .map(|sig| verify_signature(secret, body_bytes, &sig))
            .unwrap_or(false),
        None => false,
    };
    if auth.secret.is_some() && !authenticated {
        write_response(stream, "401 Unauthorized", "unauthorized")?;
        return Ok(None);
    }

    let body = String::from_utf8_lossy(body_bytes);
    let goal = extract_goal_fields(&body).and_then(|(text, author)| {
        // A body-claimed author may only elevate trust when the request was
        // cryptographically authenticated; otherwise it's recorded but stays
        // anonymous for trust purposes (can never reach `Trusted`).
        let trust_author = if authenticated { author.as_deref() } else { None };
        normalize(
            Channel::Webhook,
            &text,
            trust_author,
            None,
            auth.trusted_authors,
        )
        .map(|mut g| {
            g.author = author;
            g
        })
    });

    if goal.is_some() {
        write_response(stream, "200 OK", "accepted")?;
    } else {
        write_response(stream, "400 Bad Request", "bad req")?;
    }
    Ok(goal)
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

/// Read a full HTTP request (headers + the `Content-Length` body) off
/// `stream`, bounded to [`MAX_REQUEST_BYTES`]. Reading the exact body is
/// required for a correct HMAC over it.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        if let Some((body_start, content_len)) = header_boundary_and_len(&buf) {
            if buf.len() >= body_start.saturating_add(content_len) {
                break;
            }
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let room = MAX_REQUEST_BYTES - buf.len();
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }
    Ok(buf)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Locate the header/body boundary and parse `Content-Length`. Returns
/// `(body_start_index, content_length)`; `content_length` is 0 when absent.
fn header_boundary_and_len(buf: &[u8]) -> Option<(usize, usize)> {
    let (idx, sep) = if let Some(i) = find_subslice(buf, b"\r\n\r\n") {
        (i, 4)
    } else if let Some(i) = find_subslice(buf, b"\n\n") {
        (i, 2)
    } else {
        return None;
    };
    let content_len = parse_content_length(&String::from_utf8_lossy(&buf[..idx]));
    Some((idx + sep, content_len))
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(n) = v.trim().parse::<usize>() {
                    return n;
                }
            }
        }
    }
    0
}

fn extract_signature(headers: &str) -> Option<String> {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-arccode-signature") {
                let v = v.trim();
                let hex = v.strip_prefix("sha256=").unwrap_or(v);
                return Some(hex.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Hex-encoded HMAC-SHA256 of `body` under `secret`. Exposed so clients (and
/// tests) can produce the `X-Arccode-Signature` header value.
pub fn sign_body(secret: &[u8], body: &[u8]) -> String {
    to_hex(&hmac_sha256(secret, body))
}

/// Constant-time verification of a hex signature against `body`/`secret`.
fn verify_signature(secret: &[u8], body: &[u8], provided_hex: &str) -> bool {
    let provided = match decode_hex(provided_hex) {
        Some(b) => b,
        None => return false,
    };
    let expected = hmac_sha256(secret, body);
    if provided.len() != expected.len() {
        return false;
    }
    use subtle::ConstantTimeEq;
    expected[..].ct_eq(&provided[..]).into()
}

/// HMAC-SHA256 (RFC 2104) over `msg` with `key`, built on `sha2` so we don't
/// pull in the `hmac` crate for one call site.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Bind `addr` and serve inbound goals. Convenience wrapper over
/// [`serve_listener`]. Blocking; the daemon runs it on a dedicated thread.
pub fn serve<F>(
    addr: &str,
    auth: &WebhookAuth,
    max_requests: usize,
    on_goal: F,
) -> std::io::Result<()>
where
    F: FnMut(Goal),
{
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, auth, max_requests, on_goal)
}

/// Serve inbound goals on an already-bound `listener`. Handles
/// `max_requests` connections (0 = serve forever); each parsed goal is
/// passed to `on_goal`. Taking a pre-bound listener lets callers (and
/// tests) learn the port before any client connects, avoiding a
/// bind/connect race.
pub fn serve_listener<F>(
    listener: TcpListener,
    auth: &WebhookAuth,
    max_requests: usize,
    mut on_goal: F,
) -> std::io::Result<()>
where
    F: FnMut(Goal),
{
    let mut handled = 0usize;
    for incoming in listener.incoming() {
        let mut stream = incoming?;
        if let Ok(Some(goal)) = handle_connection(&mut stream, auth) {
            on_goal(goal);
        }
        handled += 1;
        if max_requests != 0 && handled >= max_requests {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intake::TrustLevel;
    use std::net::TcpListener as StdListener;

    #[test]
    fn parse_http_body_handles_crlf_and_lf() {
        assert_eq!(
            parse_http_body("POST /goals HTTP/1.1\r\nHost: x\r\n\r\n{\"goal\":\"hi\"}"),
            Some("{\"goal\":\"hi\"}")
        );
        assert_eq!(
            parse_http_body("POST /goals HTTP/1.1\nHost: x\n\nbody"),
            Some("body")
        );
        assert_eq!(parse_http_body("no body here"), None);
    }

    #[test]
    fn extract_goal_fields_reads_goal_and_author() {
        let (text, author) =
            extract_goal_fields(r#"{"goal":"add dark mode","author":"vedant"}"#).unwrap();
        assert_eq!(text, "add dark mode");
        assert_eq!(author.as_deref(), Some("vedant"));
    }

    #[test]
    fn extract_goal_fields_accepts_text_alias_and_rejects_empty() {
        assert_eq!(
            extract_goal_fields(r#"{"text":"fix it"}"#).unwrap().0,
            "fix it"
        );
        assert!(extract_goal_fields(r#"{"goal":"   "}"#).is_none());
        assert!(extract_goal_fields("not json").is_none());
    }

    #[test]
    fn handle_connection_parses_goal_over_loopback() {
        // Bind an ephemeral port; a client thread POSTs a goal; the
        // server-side handle_connection returns the parsed Goal.
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            let body = r#"{"goal":"add a flag","author":"vedant"}"#;
            let req = format!(
                "POST /goals HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(req.as_bytes()).unwrap();
            s.flush().unwrap();
            // Read the response so the server's write doesn't race teardown.
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp
        });

        let (mut stream, _) = listener.accept().unwrap();
        let allow = vec!["vedant".to_string()];
        let goal = handle_connection(&mut stream, &WebhookAuth::anonymous(&allow))
            .unwrap()
            .expect("goal parsed");
        assert_eq!(goal.text, "add a flag");
        assert_eq!(goal.source, Channel::Webhook);
        // The claimed author is recorded but unverified: a body-supplied
        // author must NOT elevate an unauthenticated webhook to `Trusted`,
        // even when it matches the allowlist.
        assert_eq!(goal.author.as_deref(), Some("vedant"));
        assert_eq!(goal.trust_level, TrustLevel::Untrusted);
        // Close the server side so the client's read-to-EOF returns
        // (otherwise join() below deadlocks).
        drop(stream);

        let resp = client.join().unwrap();
        assert!(resp.contains("200 OK"));
    }

    #[test]
    fn serve_listener_handles_bounded_request_count() {
        // Bind first (no rebind race), learn the port, then serve on a
        // thread while a client POSTs one goal.
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let mut goals = Vec::new();
            let allow: Vec<String> = Vec::new();
            serve_listener(listener, &WebhookAuth::anonymous(&allow), 1, |g| goals.push(g)).unwrap();
            goals
        });

        let mut s = TcpStream::connect(addr).unwrap();
        let body = r#"{"goal":"do the thing"}"#;
        let req = format!(
            "POST /goals HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);

        let goals = server.join().unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].text, "do the thing");
    }

    #[test]
    fn hmac_sign_and_verify_roundtrip() {
        let secret = b"super-secret";
        let body = br#"{"goal":"x"}"#;
        let sig = sign_body(secret, body);
        assert!(verify_signature(secret, body, &sig));
        // Wrong secret / tampered body / garbage hex all fail.
        assert!(!verify_signature(b"other", body, &sig));
        assert!(!verify_signature(secret, br#"{"goal":"y"}"#, &sig));
        assert!(!verify_signature(secret, body, "not-hex"));
        // Known-answer check against an independent HMAC-SHA256 of "" / "".
        assert_eq!(
            hmac_sha256(b"", b""),
            [
                0xb6, 0x13, 0x67, 0x9a, 0x08, 0x14, 0xd9, 0xec, 0x77, 0x2f, 0x95, 0xd7, 0x78, 0xc3,
                0x5f, 0xc5, 0xff, 0x16, 0x97, 0xc4, 0x93, 0x71, 0x56, 0x53, 0xc6, 0xc7, 0x12, 0x14,
                0x42, 0x92, 0xc5, 0xad
            ]
        );
    }

    /// POST a signed goal whose author is on the allowlist; with the matching
    /// secret configured the request authenticates and the author is trusted.
    #[test]
    fn authenticated_request_trusts_allowlisted_author() {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let secret = b"shared-secret".to_vec();

        let client_secret = secret.clone();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            let body = r#"{"goal":"ship it","author":"vedant"}"#;
            let sig = sign_body(&client_secret, body.as_bytes());
            let req = format!(
                "POST /goals HTTP/1.1\r\nHost: localhost\r\nX-Arccode-Signature: sha256={sig}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(req.as_bytes()).unwrap();
            s.flush().unwrap();
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp
        });

        let (mut stream, _) = listener.accept().unwrap();
        let allow = vec!["vedant".to_string()];
        let auth = WebhookAuth {
            secret: Some(&secret),
            trusted_authors: &allow,
        };
        let goal = handle_connection(&mut stream, &auth)
            .unwrap()
            .expect("goal parsed");
        assert_eq!(goal.text, "ship it");
        assert_eq!(goal.author.as_deref(), Some("vedant"));
        assert_eq!(goal.trust_level, TrustLevel::Trusted);
        drop(stream);

        let resp = client.join().unwrap();
        assert!(resp.contains("200 OK"));
    }

    /// With a secret configured, an unsigned (or wrongly-signed) request is
    /// rejected with 401 and yields no goal.
    #[test]
    fn unsigned_request_is_rejected_when_secret_required() {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            // No signature header, and a bogus one in a second attempt would
            // behave identically — both fail verification.
            let body = r#"{"goal":"sneak in","author":"vedant"}"#;
            let req = format!(
                "POST /goals HTTP/1.1\r\nX-Arccode-Signature: sha256=deadbeef\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            s.write_all(req.as_bytes()).unwrap();
            s.flush().unwrap();
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            resp
        });

        let (mut stream, _) = listener.accept().unwrap();
        let allow = vec!["vedant".to_string()];
        let secret = b"shared-secret".to_vec();
        let auth = WebhookAuth {
            secret: Some(&secret),
            trusted_authors: &allow,
        };
        let goal = handle_connection(&mut stream, &auth).unwrap();
        assert!(goal.is_none(), "bad signature must not yield a goal");
        drop(stream);

        let resp = client.join().unwrap();
        assert!(resp.contains("401"), "expected 401, got: {resp}");
    }
}
