//! Minimal HTTP/1.1 + SSE transport for [`crate::serve`].
//!
//! No web framework: an accept loop on `tokio::net::TcpListener`, a request
//! parser that reuses [`wingman_autonomous::webhook::header_boundary_and_len`]
//! (already load-bearing for the Slack intake listener), and helpers that
//! write responses straight to the socket.
//!
//! Handlers write their own responses rather than returning a response value.
//! That is what lets a streaming route and a JSON route have the same
//! signature: an SSE handler simply keeps writing after the headers.
//!
//! `Connection: close` on every response — one request per connection. Keep-
//! alive would buy a few milliseconds on a link that is already a tunnel or a
//! LAN hop, and cost a request-framing state machine.

use std::collections::HashMap;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Hard cap on a single request, headers included.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// How long a client has to finish sending its request before we give up.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

// Phase 1 ships the transport; the streaming and body-parsing helpers below
// are consumed by the pilot (phase 2) and turn (phase 3) routes. Marked
// rather than deleted so the transport lands reviewable in one piece.
/// Interval between SSE `:keepalive` comments. Proxies and phone radios drop
/// idle connections; a comment line is the cheapest way to stay alive and is
/// ignored by every conforming EventSource client.
#[allow(dead_code)]
pub const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// One parsed request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Percent-decoded path, without the query string.
    pub path: String,
    #[allow(dead_code)]
    pub query: HashMap<String, String>,
    /// Header names lowercased.
    pub headers: HashMap<String, String>,
    #[allow(dead_code)]
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| &**s)
    }

    /// Path split on `/`, empty segments dropped: `/v1/projects/x` → `["v1",
    /// "projects", "x"]`.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }

    #[allow(dead_code)]
    pub fn query_str(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|s| &**s)
    }

    /// `?flag`, `?flag=1`, `?flag=true`, `?flag=yes` are all true; an absent
    /// key is false.
    #[allow(dead_code)]
    pub fn query_bool(&self, key: &str) -> bool {
        match self.query.get(key).map(|s| s.trim().to_ascii_lowercase()) {
            Some(v) => v.is_empty() || v == "1" || v == "true" || v == "yes",
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn query_usize(&self, key: &str) -> Option<usize> {
        self.query.get(key)?.parse().ok()
    }

    /// Parse the body as JSON. An empty body deserialises as `null`, so a
    /// route whose body is entirely optional can use a type that accepts it.
    #[allow(dead_code)]
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        let raw = if self.body.is_empty() {
            "null"
        } else {
            std::str::from_utf8(&self.body).map_err(|e| format!("body is not utf-8: {e}"))?
        };
        serde_json::from_str(raw).map_err(|e| format!("bad JSON body: {e}"))
    }
}

/// Read one request off `sock`. `Ok(None)` means the peer hung up or sent
/// something unparseable — the caller closes the connection without a reply.
pub async fn read_request(sock: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];

    // Read headers, then exactly `Content-Length` bytes of body: a request
    // split across TCP segments must not be truncated, and a body short-read
    // would surface as a bogus parse error rather than the real cause.
    loop {
        if let Some((body_start, len)) = wingman_autonomous::webhook::header_boundary_and_len(&buf)
        {
            if buf.len() >= body_start.saturating_add(len) {
                break;
            }
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
        let n = match tokio::time::timeout(READ_TIMEOUT, sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
        };
        let room = MAX_REQUEST_BYTES.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..n.min(room)]);
    }

    let Some((body_start, len)) = wingman_autonomous::webhook::header_boundary_and_len(&buf) else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&buf[..body_start]).to_string();
    let body = buf[body_start..(body_start + len).min(buf.len())].to_vec();

    let mut lines = head.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path: percent_decode(raw_path),
        query: parse_query(raw_query.unwrap_or("")),
        headers,
        body,
    }))
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decode, treating `+` as a space (form encoding). Invalid UTF-8
/// sequences fall back to the raw text rather than failing the request.
fn percent_decode(s: &str) -> String {
    let plus_decoded = s.replace('+', " ");
    urlencoding::decode(&plus_decoded)
        .map(|c| c.into_owned())
        .unwrap_or(plus_decoded)
}

/// Write a JSON response and finish the connection.
pub async fn write_json(sock: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    write_raw(sock, status, "application/json", text.as_bytes()).await
}

/// Write `{"error": "<msg>"}` with `status`.
pub async fn write_err(sock: &mut TcpStream, status: u16, msg: &str) -> std::io::Result<()> {
    write_json(sock, status, &json!({ "error": msg })).await
}

/// Write a `text/plain` response.
#[allow(dead_code)]
pub async fn write_text(sock: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    write_raw(sock, status, "text/plain; charset=utf-8", body.as_bytes()).await
}

async fn write_raw(
    sock: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    sock.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// An open `text/event-stream`. Construct with [`Sse::start`], then push
/// events until the client goes away (any write error means it did).
#[allow(dead_code)]
pub struct Sse<'a> {
    sock: &'a mut TcpStream,
}

#[allow(dead_code)]
impl<'a> Sse<'a> {
    /// Send the SSE headers. `X-Accel-Buffering` stops nginx from holding
    /// events back, which otherwise makes a live stream look frozen.
    pub async fn start(sock: &'a mut TcpStream) -> std::io::Result<Sse<'a>> {
        let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    X-Accel-Buffering: no\r\n\
                    Connection: close\r\n\r\n";
        sock.write_all(head.as_bytes()).await?;
        sock.flush().await?;
        Ok(Sse { sock })
    }

    /// Emit one event. `data` is serialised to a single line — JSON never
    /// contains a bare newline, so no multi-line `data:` framing is needed.
    pub async fn send(&mut self, event: &str, data: &Value) -> std::io::Result<()> {
        let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".into());
        self.sock
            .write_all(format!("event: {event}\ndata: {payload}\n\n").as_bytes())
            .await?;
        self.sock.flush().await
    }

    /// Emit a comment line so an idle stream stays open.
    pub async fn keepalive(&mut self) -> std::io::Result<()> {
        self.sock.write_all(b":keepalive\n\n").await?;
        self.sock.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Request {
        // Exercise the same parsing path as `read_request` without a socket.
        let buf = raw.as_bytes().to_vec();
        let (body_start, len) = wingman_autonomous::webhook::header_boundary_and_len(&buf).unwrap();
        let head = String::from_utf8_lossy(&buf[..body_start]).to_string();
        let body = buf[body_start..(body_start + len).min(buf.len())].to_vec();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap();
        let target = parts.next().unwrap();
        let (p, q) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Request {
            method: method.to_ascii_uppercase(),
            path: percent_decode(p),
            query: parse_query(q),
            headers,
            body,
        }
    }

    #[test]
    fn parses_path_query_headers_and_body() {
        let req = parse(
            "POST /v1/projects/a%20b/turns?stream=1&tail=20 HTTP/1.1\r\n\
             Host: x\r\nAuthorization: Bearer tok\r\nContent-Length: 13\r\n\r\n\
             {\"prompt\":1}\n",
        );
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/projects/a b/turns");
        assert_eq!(req.segments(), vec!["v1", "projects", "a b", "turns"]);
        assert!(req.query_bool("stream"));
        assert_eq!(req.query_usize("tail"), Some(20));
        assert_eq!(req.header("authorization"), Some("Bearer tok"));
        assert_eq!(req.body.len(), 13);
    }

    #[test]
    fn bare_flag_query_is_true_and_absent_is_false() {
        let req = parse("GET /v1/x?compare HTTP/1.1\r\nContent-Length: 0\r\n\r\n");
        assert!(req.query_bool("compare"));
        assert!(!req.query_bool("annotate"));
    }

    #[test]
    fn empty_body_deserialises_as_null() {
        let req = parse("POST /v1/x HTTP/1.1\r\nContent-Length: 0\r\n\r\n");
        let v: Option<Value> = req.json().unwrap();
        assert!(v.is_none());
    }
}
