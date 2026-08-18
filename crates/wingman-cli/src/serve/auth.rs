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
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
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

/// Extract the presented token from either accepted header.
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
    header("x-wingman-token").map(str::trim)
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
    fn token_comparison() {
        assert!(authorized(Some("secret"), Some("secret")));
        assert!(!authorized(Some("secret"), Some("secrez")));
        assert!(!authorized(Some("secret"), Some("secretx")));
        assert!(!authorized(Some("secret"), None));
        assert!(authorized(None, None));
    }
}
