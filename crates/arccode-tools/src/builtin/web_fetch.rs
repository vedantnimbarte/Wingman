//! `web_fetch`: download a URL and return text content.
//!
//! Strips HTML tags / scripts / styles into plain text. Refuses non-HTTP(S)
//! schemes. Subject to a hard byte cap so a huge page can't blow the
//! tool-output budget.

use crate::{Tool, ToolCtx};
use arccode_core::{ToolOutcome, ToolSpec};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_BYTES: usize = 1_000_000;

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

    async fn run(&self, args: Value, _ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let url = args.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return ToolOutcome::err(
                "web_fetch only supports http:// or https:// URLs".to_string(),
            );
        }

        let client = match reqwest::Client::builder()
            .user_agent("arccode/0.0.1")
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(5))
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
        let ctx = ToolCtx::new(
            arccode_config::PermissionMode::ReadOnly,
            std::env::temp_dir(),
            std::env::temp_dir(),
        );
        let out = WebFetch
            .run(json!({"url": "file:///etc/passwd"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("http"));
    }
}
