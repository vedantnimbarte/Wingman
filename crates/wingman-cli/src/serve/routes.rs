//! Request dispatch: authenticate, route, let the handler write its own
//! response.
//!
//! Handlers take the socket rather than returning a response value, which is
//! what lets a streaming route and a JSON route share one signature — an SSE
//! handler simply keeps writing after the headers go out.

use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpStream;

use super::http::{self, Request};
use super::{auth, projects, ServeState};

/// Handle one connection start to finish.
pub async fn handle(state: Arc<ServeState>, mut sock: TcpStream) -> std::io::Result<()> {
    let Some(req) = http::read_request(&mut sock).await? else {
        return Ok(()); // unparseable or hung up; nothing useful to say back
    };

    // Health is unauthenticated on purpose: a load balancer, a phone
    // shortcut, or a `curl` sanity check should be able to ask "is it up"
    // without holding the token. It reports nothing but liveness.
    if req.path == "/v1/health" {
        return health(&state, &mut sock).await;
    }

    if !auth::authorized(state.token.as_deref(), auth::presented(|n| req.header(n))) {
        return http::write_err(&mut sock, 401, "unauthorized").await;
    }

    dispatch(&state, &req, &mut sock).await
}

async fn dispatch(
    state: &Arc<ServeState>,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    match (req.method.as_str(), req.segments().as_slice()) {
        ("GET", ["v1", "projects"]) => {
            let list: Vec<_> = state.projects.iter().map(projects::describe).collect();
            http::write_json(sock, 200, &json!({ "projects": list })).await
        }
        ("GET", ["v1", "schema"]) => http::write_json(sock, 200, &schema(state)).await,
        _ => http::write_err(sock, 404, "no such route (see GET /v1/schema)").await,
    }
}

async fn health(state: &Arc<ServeState>, sock: &mut TcpStream) -> std::io::Result<()> {
    http::write_json(
        sock,
        200,
        &json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": state.started.elapsed().as_secs(),
        }),
    )
    .await
}

/// The machine-readable route list. Generated from what dispatch actually
/// serves, so a client can discover the surface instead of pinning to a doc
/// that drifts.
fn schema(state: &Arc<ServeState>) -> serde_json::Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "ceiling": state.ceiling.to_string(),
        "routes": [
            { "method": "GET", "path": "/v1/health", "auth": false,
              "returns": "liveness, version, uptime" },
            { "method": "GET", "path": "/v1/schema", "auth": true,
              "returns": "this document" },
            { "method": "GET", "path": "/v1/projects", "auth": true,
              "returns": "allowlisted projects with branch and index state" },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;
    use wingman_config::{Config, PermissionMode};

    /// Bind an ephemeral port, serve exactly one request, and return the raw
    /// response. Exercises the real accept → parse → auth → dispatch path
    /// rather than calling the handler directly, so a mistake in the wiring
    /// shows up here instead of at runtime.
    async fn round_trip(token: Option<&str>, request: &str) -> String {
        let state = Arc::new(ServeState {
            cfg: Config::default(),
            projects: Vec::new(),
            token: token.map(str::to_string),
            ceiling: PermissionMode::AutoEdit,
            started: Instant::now(),
            turns: Semaphore::new(1),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle(state, sock).await.unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        server.await.unwrap();
        String::from_utf8_lossy(&buf).to_string()
    }

    #[tokio::test]
    async fn health_needs_no_token() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"ok\":true"), "{resp}");
    }

    #[tokio::test]
    async fn other_routes_reject_a_missing_token() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/projects HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
        // The body says nothing about why, so it cannot confirm a guess.
        assert!(resp.contains("unauthorized"), "{resp}");
        assert!(!resp.contains("sekrit"), "{resp}");
    }

    #[tokio::test]
    async fn a_good_token_reaches_the_route() {
        let resp = round_trip(
            Some("sekrit"),
            "GET /v1/projects HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sekrit\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"projects\""), "{resp}");
    }

    #[tokio::test]
    async fn unknown_route_is_404_and_points_at_the_schema() {
        let resp = round_trip(
            None,
            "GET /v1/nope HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        assert!(resp.contains("/v1/schema"), "{resp}");
    }
}
