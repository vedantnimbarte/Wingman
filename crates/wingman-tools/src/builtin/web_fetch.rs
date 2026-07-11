//! `web_fetch`: download a URL and return text content.
//!
//! Strips HTML tags / scripts / styles into plain text. Refuses non-HTTP(S)
//! schemes. Subject to a hard byte cap so a huge page can't blow the
//! tool-output budget.

use crate::{Tool, ToolCtx};
use wingman_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

const MAX_BYTES: usize = 1_000_000;
const MAX_REDIRECTS: usize = 5;

pub struct WebFetch;

#[derive(Debug, Deserialize)]
struct Args {
    url: String,
    /// Optional prompt for downstream consumers — recorded but not used here.
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,
    /// Return raw body without HTML stripping.
    #[serde(default)]
    raw: bool,
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description:
                "Fetch a URL over HTTPS and return its text body. HTML is stripped to plain text \
                 unless `raw` is true. Capped at ~1 MB. Refuses non-http(s) schemes."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s):// URL." },
                    "prompt": { "type": "string", "description": "Optional context note." },
                    "raw": { "type": "boolean", "default": false, "description": "Return raw body, no HTML stripping." }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        if !ctx.allows_network() {
            return ToolOutcome::err(
                "network access denied: web_fetch requires auto-edit or yolo mode".to_string(),
            );
        }
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let url = args.url.trim();
        let parsed = match reqwest::Url::parse(url) {
            Ok(u) => u,
            Err(e) => return ToolOutcome::err(format!("invalid URL: {e}")),
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return ToolOutcome::err(
                "web_fetch only supports http:// or https:// URLs".to_string(),
            );
        }

        // SSRF guard: refuse loopback / private / link-local targets up front
        // so the model (or prompt-injected web content) can't reach the cloud
        // metadata endpoint or internal services and exfiltrate the response.
        if let Err(e) = validate_public_host(&parsed) {
            return ToolOutcome::err(e);
        }

        // Re-validate every redirect hop — a public URL must not be able to
        // bounce us onto an internal address.
        let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            match validate_public_host(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(msg) => attempt.error(msg),
            }
        });

        let client = match reqwest::Client::builder()
            .user_agent("wingman/0.0.1")
            .timeout(std::time::Duration::from_secs(20))
            .redirect(redirect_policy)
            .dns_resolver(Arc::new(PublicOnlyResolver))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolOutcome::err(format!("http client init: {e}")),
        };

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolOutcome::err(format!("fetch {url}: {e}")),
        };
        let status = resp.status();
        let final_url = resp.url().to_string();
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolOutcome::err(format!("read body: {e}")),
        };
        let truncated = bytes.len() > MAX_BYTES;
        let slice = &bytes[..bytes.len().min(MAX_BYTES)];
        let text = String::from_utf8_lossy(slice).into_owned();

        let body = if args.raw { text } else { strip_html(&text) };

        let header = format!(
            "url: {final_url}\nstatus: {status}{trunc}\n---\n",
            trunc = if truncated { "  [truncated]" } else { "" }
        );
        ToolOutcome::ok(format!("{header}{body}"))
    }
}

/// Reject a URL whose host is — or resolves to — an address the agent has no
/// business reaching: loopback, RFC-1918 private ranges, link-local (which
/// includes the `169.254.169.254` cloud-metadata endpoint), CGNAT, and the
/// IPv6 equivalents. IP-literal hosts are checked directly; named hosts are
/// resolved and *every* returned address must be public.
///
/// Note: this resolves the name once here; reqwest resolves again at connect
/// time, so a determined DNS-rebinding attacker retains a narrow TOCTOU
/// window. Blocking IP literals and named-host resolution shuts the common
/// vectors (metadata IP, `localhost`, redirect-to-internal).
fn validate_public_host(url: &reqwest::Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Direct IP literal (e.g. http://169.254.169.254/…).
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if ip_is_blocked(ip) {
            Err(format!("refusing to fetch non-public address {host}"))
        } else {
            Ok(())
        };
    }

    // Reject localhost spellings before trusting the resolver.
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return Err("refusing to fetch localhost".to_string());
    }

    let port = url.port_or_known_default().unwrap_or(443);
    let mut resolved = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .peekable();
    if resolved.peek().is_none() {
        return Err(format!("no addresses resolved for {host}"));
    }
    for sa in resolved {
        if ip_is_blocked(sa.ip()) {
            return Err(format!(
                "refusing to fetch {host}: resolves to non-public address {}",
                sa.ip()
            ));
        }
    }
    Ok(())
}

/// A reqwest DNS resolver that strips every non-public address from a
/// lookup, so the IP reqwest ultimately connects to is guaranteed public.
///
/// This is what closes the DNS-rebinding TOCTOU: `validate_public_host`
/// resolves once up front (for a friendly early error and to reject IP
/// literals, which never reach DNS), but a hostname that flips to an
/// internal address between that check and connect time is still caught
/// here, because reqwest only ever connects to addresses this resolver
/// hands back. If nothing public remains, the connection fails closed.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            // `to_socket_addrs` is blocking; keep it off the async reactor.
            let lookup = tokio::task::spawn_blocking(move || {
                (host.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|it| it.collect::<Vec<SocketAddr>>())
            })
            .await;

            type BoxErr = Box<dyn std::error::Error + Send + Sync>;
            let addrs = match lookup {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => return Err(Box::new(e) as BoxErr),
                Err(e) => return Err(Box::new(e) as BoxErr),
            };
            let public: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|sa| !ip_is_blocked(sa.ip()))
                .collect();
            if public.is_empty() {
                return Err(
                    "refusing to connect: host resolves only to non-public addresses".into(),
                );
            }
            let iter: reqwest::dns::Addrs = Box::new(public.into_iter());
            Ok(iter)
        })
    }
}

/// True for any address the SSRF guard should refuse.
fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_blocked(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || is_unique_local_v6(v6)        // fc00::/7
                || is_unicast_link_local_v6(v6)  // fe80::/10
                // Unwrap IPv4-mapped / -compatible addresses and re-check.
                || v6
                    .to_ipv4()
                    .map(v4_is_blocked)
                    .unwrap_or(false)
        }
    }
}

fn v4_is_blocked(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()       // 169.254.0.0/16 — includes metadata IP
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || o[0] == 0                // 0.0.0.0/8 "this network"
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000) // 100.64/10 CGNAT
}

fn is_unique_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

fn is_unicast_link_local_v6(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// Cheap HTML-to-text: drop <script>/<style> blocks, then strip tags and
/// collapse whitespace. Not a real parser — good enough to feed to a model.
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let lower = s.to_ascii_lowercase();
    let mut i = 0;
    while i < bytes.len() {
        // Skip <script>...</script> and <style>...</style>.
        if lower[i..].starts_with("<script") {
            if let Some(end) = lower[i..].find("</script>") {
                i += end + "</script>".len();
                continue;
            } else {
                break;
            }
        }
        if lower[i..].starts_with("<style") {
            if let Some(end) = lower[i..].find("</style>") {
                i += end + "</style>".len();
                continue;
            } else {
                break;
            }
        }
        if bytes[i] == b'<' {
            // Skip until '>'
            if let Some(end) = s[i..].find('>') {
                i += end + 1;
                out.push(' ');
                continue;
            } else {
                break;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    // Decode a few common entities.
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // Collapse whitespace.
    let mut compact = String::with_capacity(out.len());
    let mut prev_blank = false;
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !prev_blank {
                compact.push('\n');
                prev_blank = true;
            }
        } else {
            compact.push_str(t);
            compact.push('\n');
            prev_blank = false;
        }
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_drops_tags_and_scripts() {
        let html = r#"<html><head><script>alert(1)</script><style>.x{}</style></head>
            <body><p>Hello <b>world</b></p><p>line 2</p></body></html>"#;
        let stripped = strip_html(html);
        assert!(!stripped.contains("alert"));
        assert!(!stripped.contains(".x"));
        assert!(stripped.contains("Hello"));
        assert!(stripped.contains("world"));
        assert!(stripped.contains("line 2"));
    }

    #[tokio::test]
    async fn refuses_non_http_scheme() {
        // Yolo so the network gate passes and we reach the scheme check.
        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::Yolo,
            std::env::temp_dir(),
            std::env::temp_dir(),
        );
        let out = WebFetch
            .run(json!({"url": "file:///etc/passwd"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("http"));
    }

    #[tokio::test]
    async fn refuses_network_in_read_only() {
        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::ReadOnly,
            std::env::temp_dir(),
            std::env::temp_dir(),
        );
        let out = WebFetch
            .run(json!({"url": "https://example.com/"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("network access denied"));
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn blocks_cloud_metadata_and_loopback() {
        // The classic SSRF target: AWS/GCP/Azure metadata service.
        assert!(validate_public_host(&url("http://169.254.169.254/latest/meta-data/")).is_err());
        assert!(validate_public_host(&url("http://127.0.0.1/")).is_err());
        assert!(validate_public_host(&url("http://[::1]:8080/")).is_err());
        assert!(validate_public_host(&url("http://localhost/admin")).is_err());
        assert!(validate_public_host(&url("http://10.0.0.5/")).is_err());
        assert!(validate_public_host(&url("http://192.168.1.1/")).is_err());
        assert!(validate_public_host(&url("http://172.16.4.4/")).is_err());
        assert!(validate_public_host(&url("http://0.0.0.0/")).is_err());
    }

    #[test]
    fn allows_public_ip_literal() {
        // A public IP literal (Cloudflare DNS) must pass the host filter.
        assert!(validate_public_host(&url("https://1.1.1.1/")).is_ok());
    }

    #[tokio::test]
    async fn run_refuses_metadata_endpoint() {
        // Yolo so the network gate passes and we reach the SSRF host check.
        let ctx = ToolCtx::new(
            wingman_config::PermissionMode::Yolo,
            std::env::temp_dir(),
            std::env::temp_dir(),
        );
        let out = WebFetch
            .run(
                json!({"url": "http://169.254.169.254/latest/meta-data/iam/"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("non-public"));
    }
}
