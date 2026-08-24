//! The web control panel's static shell.
//!
//! Three files, embedded at compile time by `build.rs` and served from memory:
//! `index.html`, `app.js`, `app.css`. No runtime file dependency, so
//! `wingman serve` stays a single static binary.
//!
//! **The shell is served without a token, and the API is not.** A browser has
//! to load the page before it can present a credential, so gating the shell
//! would be a chicken-and-egg with no upside: these three files contain no
//! project data, no config, and no run state — every byte of that is behind
//! `/v1`, which is authenticated as it always was. What an unauthenticated
//! request learns here is that something is listening, which `GET /v1/health`
//! already tells it.
//!
//! Unknown paths fall back to `index.html` so client-side routes deep-link.
//! That fallback stops at `/v1`: an unknown API path must stay a JSON 404
//! rather than becoming a 200 with an HTML body, which is the failure mode
//! that makes a client's error handling silently wrong.

use tokio::net::TcpStream;

use super::http::{self, Request};

const INDEX_HTML: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui/index.html"));
const APP_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui/app.js"));
const APP_CSS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui/app.css"));

/// Whether a real bundle was embedded, or the `build.rs` placeholder stood in
/// for a missing `ui/dist/`. Reported at startup so a stub page is never a
/// mystery.
pub fn embedded() -> bool {
    env!("WINGMAN_UI_EMBEDDED") == "true"
}

/// Does this request belong to the shell rather than the API?
///
/// Called before the auth gate, so it must be conservative: anything under
/// `/v1` is the API's, and anything that is not a `GET` is not a static asset.
///
/// Takes the parts rather than the `Request` so the decision is testable
/// without a parsed socket read.
pub fn is_shell(method: &str, segments: &[&str]) -> bool {
    method == "GET" && segments.first() != Some(&"v1")
}

/// Serve one shell request. Assumes [`is_shell`] already said yes.
pub async fn serve(req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let (body, content_type) = match req.segments().as_slice() {
        ["app.js"] => (APP_JS, "text/javascript; charset=utf-8"),
        ["app.css"] => (APP_CSS, "text/css; charset=utf-8"),
        // Everything else, `/` included, is the app shell: the client router
        // reads the path and renders the right view.
        _ => (INDEX_HTML, "text/html; charset=utf-8"),
    };

    let etag = etag(body);

    // A conditional request that already has this exact body gets a 304. The
    // tag is a hash of the bytes rather than the crate version, so rebuilding
    // the bundle without bumping the version still invalidates the cache —
    // the version would be stale for exactly the person iterating on the UI.
    if req.header("if-none-match") == Some(etag.as_str()) {
        return http::write_raw(sock, 304, content_type, &[("ETag", &etag)], &[]).await;
    }

    // `no-cache` means revalidate, not "never cache": the browser keeps the
    // bytes and asks with `If-None-Match`, so the common case is a 304 rather
    // than a re-download, and a new binary is picked up immediately.
    http::write_raw(
        sock,
        200,
        content_type,
        &[("ETag", &etag), ("Cache-Control", "no-cache")],
        body,
    )
    .await
}

/// A strong `ETag` over the asset bytes.
///
/// `DefaultHasher` is not stable across Rust releases and is not
/// cryptographic. Neither matters: the tag only has to change when the bytes
/// change within one running binary, and a client that gets a false miss
/// re-downloads a file it is already holding in memory.
fn etag(body: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths_are_never_the_shell() {
        assert!(!is_shell("GET", &["v1", "health"]));
        assert!(!is_shell("GET", &["v1", "projects", "x", "cost"]));
        // An unknown API path must stay a JSON 404, not become the app shell.
        assert!(!is_shell("GET", &["v1", "nope"]));
    }

    #[test]
    fn non_get_is_never_the_shell() {
        assert!(!is_shell("POST", &[]));
        assert!(!is_shell("DELETE", &["app.js"]));
    }

    #[test]
    fn root_and_client_routes_are_the_shell() {
        assert!(is_shell("GET", &[])); // `/`
        assert!(is_shell("GET", &["board"]));
        assert!(is_shell("GET", &["config", "pilot"]));
        assert!(is_shell("GET", &["app.js"]));
    }

    /// Release gate, not a unit test — `#[ignore]`d so `cargo test` stays
    /// green for a contributor who has never run npm, and run explicitly by
    /// the `web-ui` CI job after it builds the bundle.
    ///
    /// Without it, a broken UI build ships a binary that quietly serves the
    /// `build.rs` placeholder and passes every other check.
    #[test]
    #[ignore = "requires `npm run build` in ui/ first; CI runs it with --ignored"]
    fn ui_bundle_is_embedded() {
        assert!(
            embedded(),
            "ui/dist was missing at compile time, so the placeholder was embedded instead of \
             the panel. Run `npm ci && npm run build` in ui/, then rebuild."
        );
        assert!(
            APP_JS.len() > 1024,
            "app.js is {} bytes — that is not a real bundle",
            APP_JS.len()
        );
        assert!(
            INDEX_HTML.windows(8).any(|w| w == b"/app.js\""),
            "index.html does not reference /app.js; the Vite output names drifted from build.rs"
        );
    }

    #[test]
    fn etag_tracks_the_bytes() {
        assert_eq!(etag(b"same"), etag(b"same"));
        assert_ne!(etag(b"one"), etag(b"two"));
        // Quoted, per RFC 7232 — an unquoted tag never matches.
        assert!(etag(b"x").starts_with('"') && etag(b"x").ends_with('"'));
    }
}
