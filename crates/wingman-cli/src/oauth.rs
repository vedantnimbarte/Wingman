//! OAuth 2.0 PKCE browser flow for ChatGPT subscription authentication.
//!
//! Uses OpenAI's official public client id (`app_EMoamEEZ73f0CkXaXp7hrann`)
//! and the same PKCE flow used by the Codex CLI, so no developer credentials
//! are needed. The user authenticates in their default browser; a short-lived
//! local HTTP server on port 1455 receives the callback.
//!
//! Returns `(access_token, refresh_token)` on success. Both tokens are stored
//! by the caller in the OS keychain (`"chatgpt"` and `"chatgpt_refresh"`).
//!
//! Token refresh: call [`refresh_chatgpt_token`] with a stored refresh token
//! to get a new access token without re-opening the browser.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/callback";
const SCOPES: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;

/// Run the full browser-based PKCE login flow.
///
/// Returns `(access_token, refresh_token)`.  The caller is responsible for
/// storing both in the keychain.
pub async fn chatgpt_oauth_login() -> Result<(String, String)> {
    // 1. Generate PKCE parameters.
    let (code_verifier, code_challenge) = generate_pkce();

    // 2. Random state for CSRF protection.
    let state = {
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        URL_SAFE_NO_PAD.encode(buf)
    };

    // 3. Build authorization URL.
    let auth_url = format!(
        "{AUTH_URL}?response_type=code\
         &client_id={CLIENT_ID}\
         &redirect_uri={redirect}\
         &scope={scope}\
         &state={state}\
         &code_challenge={challenge}\
         &code_challenge_method=S256",
        redirect = urlencoding::encode(REDIRECT_URI),
        scope = urlencoding::encode(SCOPES),
        state = state,
        challenge = code_challenge,
    );

    // 4. Open default browser.
    eprintln!("\nwingman: opening browser for ChatGPT authentication…");
    eprintln!("If the browser did not open, visit:\n  {auth_url}\n");
    open_browser(&auth_url);

    // 5. Wait for callback on :1455.
    let (auth_code, returned_state) = wait_for_callback(CALLBACK_PORT)
        .await
        .context("waiting for OAuth callback")?;

    if returned_state != state {
        bail!("OAuth state mismatch — possible CSRF; please retry");
    }

    // 6. Exchange authorization code for tokens.
    let http = reqwest::Client::new();
    let token_resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": auth_code,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": code_verifier,
        }))
        .send()
        .await
        .context("token exchange request")?;

    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let body = token_resp.text().await.unwrap_or_default();
        bail!("token exchange failed ({status}): {body}");
    }

    let tokens: serde_json::Value = token_resp.json().await.context("token response json")?;

    let access_token = tokens["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("token response missing access_token"))?
        .to_string();

    let refresh_token = tokens["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow!("token response missing refresh_token"))?
        .to_string();

    Ok((access_token, refresh_token))
}

/// Use a stored refresh token to obtain a new access token without re-opening
/// the browser.  Returns `(new_access_token, new_refresh_token)`.
pub async fn refresh_chatgpt_token(refresh_token: &str) -> Result<(String, String)> {
    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("token refresh request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("token refresh failed ({status}): {body}");
    }

    let tokens: serde_json::Value = resp.json().await.context("refresh response json")?;

    let access_token = tokens["access_token"]
        .as_str()
        .ok_or_else(|| anyhow!("refresh response missing access_token"))?
        .to_string();

    // Some servers rotate the refresh token; fall back to the original if not.
    let new_refresh = tokens["refresh_token"]
        .as_str()
        .unwrap_or(refresh_token)
        .to_string();

    Ok((access_token, new_refresh))
}

/// Decode the `exp` claim (Unix seconds) from a JWT without verifying the
/// signature.  Returns `None` if the token is not a valid JWT or has no `exp`.
pub fn jwt_exp(token: &str) -> Option<u64> {
    let payload_b64 = token.split('.').nth(1)?;
    // JWT base64url may lack padding — add it back.
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64.to_string(),
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(payload_b64)
        // Fallback: try standard base64 with padding.
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&padded))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims["exp"].as_u64()
}

/// Returns `true` if the token is expired or will expire within `margin_secs`.
pub fn token_is_expiring(token: &str, margin_secs: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match jwt_exp(token) {
        Some(exp) => now + margin_secs >= exp,
        None => false, // Can't tell — assume valid.
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    let code_challenge = URL_SAFE_NO_PAD.encode(hash);

    (code_verifier, code_challenge)
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Start a local TCP listener and wait for the OAuth redirect.  Returns
/// `(code, state)` extracted from the GET query string.
async fn wait_for_callback(port: u16) -> Result<(String, String)> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("binding callback listener on port {port}"))?;

    let (mut stream, _addr) = listener.accept().await.context("accepting callback")?;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.context("reading callback")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // HTTP request first line: "GET /callback?code=…&state=… HTTP/1.1"
    let first_line = request.lines().next().unwrap_or("");
    let query_str = first_line
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.split_once('?').map(|(_, q)| q))
        .unwrap_or("");

    let code = extract_query_param(query_str, "code")
        .ok_or_else(|| anyhow!("callback missing 'code' parameter"))?;
    let state = extract_query_param(query_str, "state").unwrap_or_default();

    // Send a friendly success page.
    let html = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/html\r\n",
        "Connection: close\r\n",
        "\r\n",
        "<!DOCTYPE html><html><body>",
        "<h2>wingman: authentication complete</h2>",
        "<p>You can close this tab and return to the terminal.</p>",
        "</body></html>",
    );
    let _ = stream.write_all(html.as_bytes()).await;

    Ok((code, state))
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                // Decode percent-encoded characters (simple single-pass).
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    result.push(byte as char);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            result.push(' ');
            i += 1;
            continue;
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_lengths() {
        let (verifier, challenge) = generate_pkce();
        // base64url of 32 bytes = 43 chars (no padding).
        assert_eq!(verifier.len(), 43);
        // SHA256 = 32 bytes → base64url = 43 chars.
        assert_eq!(challenge.len(), 43);
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn extract_param() {
        assert_eq!(
            extract_query_param("code=abc123&state=xyz", "code"),
            Some("abc123".into())
        );
        assert_eq!(
            extract_query_param("code=abc123&state=xyz", "state"),
            Some("xyz".into())
        );
        assert_eq!(extract_query_param("code=abc123", "missing"), None);
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }
}
