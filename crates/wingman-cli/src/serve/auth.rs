//! Bearer-token authentication and the startup refusals that keep a
//! misconfigured server from being an open door.
//!
//! One token, compared in constant time. Per-token scopes are a stated
//! non-goal (`docs/HTTP-API.md`): run two servers with different ceilings if
//! you need two authorities.

use std::net::SocketAddr;

use anyhow::{anyhow, Result};
use wingman_config::{PermissionMode, ServeConfig, MIN_REMOTE_TOKEN_LEN};

/// Keyring entry name for the API token, alongside the provider keys.
pub const KEYRING_ENTRY: &str = "serve-token";

/// Resolve the effective token: an explicit value from config (already
/// `${ENV_VAR}`-expanded at load), the keyring when config says `"keyring"`,
/// or the keyring as a fallback when config says nothing — so
/// `wingman serve --init-token` followed by `wingman serve` just works.
pub fn resolve_token(cfg: &ServeConfig) -> Option<String> {
    match cfg.token.as_deref().map(str::trim) {
        Some("keyring") | Some("") | None => wingman_config::secrets::load(KEYRING_ENTRY)
            .ok()
            .flatten()
            .filter(|t| !t.trim().is_empty()),
        Some(explicit) => Some(explicit.to_string()),
    }
}

/// Generate a fresh 32-byte token, URL-safe base64 (43 chars).
pub fn generate_token() -> String {
    use base64::Engine;
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Validate the bind address and token together. Returns the parsed address.
///
/// The refusals here are the difference between "an API on my laptop" and
/// "a coding agent with write access, reachable from the network, with no
/// password". Each one is a hard exit rather than a warning, because a
/// warning on a daemon nobody is watching is not a control.
pub fn check_bind(
    addr: &str,
    token: Option<&str>,
    ceiling: PermissionMode,
    allow_yolo: bool,
) -> Result<SocketAddr> {
    let parsed: SocketAddr = addr
        .parse()
        .map_err(|e| anyhow!("[serve].addr '{addr}' is not an ip:port address: {e}"))?;
    let loopback = parsed.ip().is_loopback();

    match token {
        None if !loopback => {
            return Err(anyhow!(
                "refusing to start: [serve].addr binds {addr}, which is reachable from the \
                 network, and no token is set.\n\
                 An unauthenticated API is remote control of a coding agent with write access.\n\
                 Run `wingman serve --init-token` (stores one in the OS keyring), or set \
                 [serve].token."
            ));
        }
        Some(t) if !loopback && t.len() < MIN_REMOTE_TOKEN_LEN => {
            return Err(anyhow!(
                "refusing to start: the token is {} characters; a non-loopback bind requires at \
                 least {MIN_REMOTE_TOKEN_LEN}.\n\
                 `wingman serve --init-token` generates a 43-character one.",
                t.len()
            ));
        }
        _ => {}
    }

    if ceiling == PermissionMode::Yolo && !allow_yolo {
        return Err(anyhow!(
            "refusing to start: [serve].max_permission_mode = \"yolo\" means any request can run \
             arbitrary shell commands on this machine.\n\
             If that is genuinely what you want, pass --allow-yolo so it is a deliberate act at \
             launch rather than a config line."
        ));
    }

    Ok(parsed)
}

/// Name of the cookie the web panel authenticates with. See [`cookie`].
pub const COOKIE_NAME: &str = "wingman_token";

/// Extract the presented token from any accepted source: the `Authorization`
/// header, the `X-Wingman-Token` header, or the panel's cookie.
///
/// Headers win over the cookie. A script or CI job that sends an explicit
/// credential means it, and should not have a stale browser cookie silently
/// substituted for the token it just supplied.
pub fn presented<'a>(header: impl Fn(&str) -> Option<&'a str>) -> Option<&'a str> {
    if let Some(v) = header("authorization") {
        // Case-insensitive scheme, per RFC 7235.
        let mut parts = v.splitn(2, ' ');
        if let (Some(scheme), Some(rest)) = (parts.next(), parts.next()) {
            if scheme.eq_ignore_ascii_case("bearer") {
                return Some(rest.trim());
            }
        }
        return None;
    }
    if let Some(v) = header("x-wingman-token") {
        return Some(v.trim());
    }
    header("cookie").and_then(cookie_value)
}

/// Pull [`COOKIE_NAME`] out of a `Cookie:` header.
///
/// Hand-parsed rather than pulled in as a dependency: the header is a
/// `; `-separated list of `name=value`, and the panel sets a base64 token with
/// no quoting or encoding to undo.
fn cookie_value(header: &str) -> Option<&str> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| value.trim())
    })
}

/// The `Set-Cookie` value that signs the browser in, or signs it out when
/// `token` is `None`.
///
/// **`HttpOnly`** is the point of the whole exercise: the panel will grow an
/// npm dependency tree, and a token readable by page script is one bad
/// transitive dependency away from being exfiltrated. **`SameSite=Strict`** is
/// what stands in for CSRF tokens — no cross-site request carries this cookie,
/// and no CORS headers are set, so another origin can neither send it nor read
/// the reply.
///
/// **`Secure` is deliberately absent.** It would be correct over TLS and wrong
/// here: the panel is reached over plain HTTP on loopback or a LAN address,
/// which is exactly the phone-on-the-sofa case this exists for, and a `Secure`
/// cookie on those origins is simply discarded. The threat it defends against
/// — someone reading the wire — already sees `Authorization: Bearer` on every
/// other request to the same daemon.
///
/// The cookie carries the token itself rather than a session id, so there is
/// no session table and no expiry bookkeeping. The ceiling that accepts: a
/// leaked cookie is a leaked token, and the only revocation is
/// `wingman serve --init-token` to rotate it.
pub fn cookie(token: Option<&str>) -> String {
    match token {
        Some(t) => format!(
            "{COOKIE_NAME}={t}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            60 * 60 * 24 * 30
        ),
        None => format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    }
}

/// Constant-time comparison of the presented token against the configured
/// one. A server with no token configured (loopback only, by `check_bind`)
/// accepts every request.
pub fn authorized(configured: Option<&str>, presented: Option<&str>) -> bool {
    let Some(expected) = configured else {
        return true;
    };
    let Some(got) = presented else {
        return false;
    };
    use subtle::ConstantTimeEq;
    // Lengths are compared first and leak the token length, which for a
    // random 43-char token tells an attacker nothing they did not already
    // know from the docs.
    expected.len() == got.len() && expected.as_bytes().ct_eq(got.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_without_token_is_allowed_but_public_is_not() {
        assert!(check_bind("127.0.0.1:8787", None, PermissionMode::AutoEdit, false).is_ok());
        assert!(check_bind("0.0.0.0:8787", None, PermissionMode::AutoEdit, false).is_err());
    }

    #[test]
    fn short_token_rejected_only_off_loopback() {
        assert!(check_bind(
            "127.0.0.1:8787",
            Some("short"),
            PermissionMode::AutoEdit,
            false
        )
        .is_ok());
        assert!(check_bind(
            "0.0.0.0:8787",
            Some("short"),
            PermissionMode::AutoEdit,
            false
        )
        .is_err());
        let long = generate_token();
        assert!(long.len() >= MIN_REMOTE_TOKEN_LEN);
        assert!(check_bind("0.0.0.0:8787", Some(&long), PermissionMode::AutoEdit, false).is_ok());
    }

    #[test]
    fn yolo_ceiling_needs_the_flag() {
        let t = generate_token();
        assert!(check_bind("127.0.0.1:8787", Some(&t), PermissionMode::Yolo, false).is_err());
        assert!(check_bind("127.0.0.1:8787", Some(&t), PermissionMode::Yolo, true).is_ok());
    }

    #[test]
    fn bad_addr_is_rejected() {
        assert!(check_bind("not-an-addr", None, PermissionMode::AutoEdit, false).is_err());
    }

    #[test]
    fn bearer_and_custom_header_both_parse() {
        let bearer = |n: &str| (n == "authorization").then_some("Bearer abc");
        assert_eq!(presented(bearer), Some("abc"));
        let lower = |n: &str| (n == "authorization").then_some("bearer abc");
        assert_eq!(presented(lower), Some("abc"));
        let custom = |n: &str| (n == "x-wingman-token").then_some("abc");
        assert_eq!(presented(custom), Some("abc"));
        let basic = |n: &str| (n == "authorization").then_some("Basic abc");
        assert_eq!(presented(basic), None);
    }

    #[test]
    fn the_cookie_is_read_when_no_header_is_present() {
        let c = |n: &str| (n == "cookie").then_some("theme=dark; wingman_token=abc; x=1");
        assert_eq!(presented(c), Some("abc"));

        let only = |n: &str| (n == "cookie").then_some("wingman_token=abc");
        assert_eq!(presented(only), Some("abc"));

        let absent = |n: &str| (n == "cookie").then_some("theme=dark");
        assert_eq!(presented(absent), None);
    }

    #[test]
    fn an_explicit_header_wins_over_a_stale_cookie() {
        let both = |n: &str| match n {
            "authorization" => Some("Bearer from-header"),
            "cookie" => Some("wingman_token=from-cookie"),
            _ => None,
        };
        assert_eq!(presented(both), Some("from-header"));
    }

    #[test]
    fn the_cookie_never_loosens_its_flags() {
        let set = cookie(Some("abc"));
        assert!(set.contains("wingman_token=abc"));
        assert!(set.contains("HttpOnly"), "{set}");
        assert!(set.contains("SameSite=Strict"), "{set}");
        // `Secure` would be discarded by the browser on the plain-HTTP LAN
        // origin the panel is served from, silently breaking sign-in.
        assert!(!set.contains("Secure"), "{set}");
    }

    #[test]
    fn signing_out_expires_the_cookie_rather_than_blanking_it() {
        let cleared = cookie(None);
        assert!(cleared.contains("Max-Age=0"), "{cleared}");
        assert!(cleared.contains("HttpOnly"), "{cleared}");
    }

    #[test]
    fn token_comparison() {
        assert!(authorized(Some("secret"), Some("secret")));
        assert!(!authorized(Some("secret"), Some("secrez")));
        assert!(!authorized(Some("secret"), Some("secretx")));
        assert!(!authorized(Some("secret"), None));
        assert!(authorized(None, None));
    }
}
