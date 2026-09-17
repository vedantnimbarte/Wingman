//! `browser`: drive a headless Chrome tab across calls.
//!
//! One tool with an `action` rather than six `browser_*` tools: every schema
//! is paid for on every request, and the six share all of their arguments.
//!
//! The browser is launched on first use and belongs to this tool instance, so
//! it lives as long as the session's registry and Chrome is killed when that
//! is dropped — the same `Drop`-based cleanup background jobs rely on.
//! ponytail: a subagent builds its own registry and so gets its own browser
//! (launched only if it uses one); share it through `ToolCtx` like `jobs` if
//! parent and child ever need the same page.
//!
//! Only registered in builds with the `browser` feature; see
//! `wingman-cli`'s `base_registry`. The policy and argument handling below
//! compile everywhere so they are tested in the default build.

use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::IpAddr;
use wingman_core::{ToolOutcome, ToolSpec};

/// Page text returned per call. Enough for a dev page's visible content; a
/// long document is better read with `eval` against a selector.
#[cfg_attr(not(feature = "browser"), allow(dead_code))] // used by the Chrome path
const MAX_TEXT_BYTES: usize = 16 * 1024;
/// `eval` results and console dumps.
#[cfg_attr(not(feature = "browser"), allow(dead_code))] // used by the Chrome path
const MAX_EVAL_BYTES: usize = 8 * 1024;

pub struct Browser {
    local_only: bool,
    #[cfg(feature = "browser")]
    session: std::sync::Arc<std::sync::Mutex<Option<wingman_browser::Session>>>,
}

impl Browser {
    /// `local_only` mirrors `[privacy].local_only`: only loopback URLs open,
    /// and Chrome is launched unable to reach anything else.
    pub fn new(local_only: bool) -> Self {
        Self {
            local_only,
            #[cfg(feature = "browser")]
            session: Default::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    expression: Option<String>,
}

#[derive(Debug, PartialEq)]
enum Action {
    Navigate(String),
    Click(String),
    Type(String, String),
    Screenshot,
    Console,
    Eval(String),
}

fn parse(args: Value) -> Result<Action, String> {
    let a: Args = serde_json::from_value(args).map_err(|e| format!("invalid args: {e}"))?;
    let need = |v: Option<String>, field: &str| -> Result<String, String> {
        match v {
            Some(s) if !s.trim().is_empty() => Ok(s),
            _ => Err(format!("`{}` requires a non-empty `{field}`", a.action)),
        }
    };
    Ok(match a.action.as_str() {
        "navigate" => Action::Navigate(need(a.url.clone(), "url")?.trim().to_string()),
        "click" => Action::Click(need(a.selector.clone(), "selector")?),
        // Typing an empty string is legitimate (focus a field), so `text` is
        // only required to be present.
        "type" => Action::Type(
            need(a.selector.clone(), "selector")?,
            a.text
                .clone()
                .ok_or_else(|| "`type` requires `text`".to_string())?,
        ),
        "screenshot" => Action::Screenshot,
        "console" => Action::Console,
        "eval" => Action::Eval(need(a.expression.clone(), "expression")?),
        other => {
            return Err(format!(
                "unknown action `{other}` (navigate, click, type, screenshot, console, eval)"
            ))
        }
    })
}

/// May the browser open (or stay on) `url`?
///
/// - http(s) only: `file://` would read any file on disk past path
///   containment, and `javascript:` / `chrome://` are not pages.
/// - Under `local_only`, loopback only — the dev-server case.
/// - Never the link-local range, which holds the cloud metadata endpoint
///   `web_fetch` also refuses. Unlike `web_fetch`, loopback and private
///   addresses are allowed: a dev server is the point of this tool.
///   ponytail: IP literals only; a DNS name resolving to 169.254.x.x is not
///   caught. Resolve and check like `web_fetch` if that matters.
fn check_url(url: &str, local_only: bool) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("browser only opens http:// or https:// URLs".into());
    }
    let raw = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    // IPv6 hosts come back bracketed.
    let Ok(host) = raw
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    else {
        let d = raw.trim_end_matches('.').to_ascii_lowercase();
        if local_only && d != "localhost" && !d.ends_with(".localhost") {
            return Err(local_only_refusal(url));
        }
        return Ok(());
    };
    let host = match host {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(host),
        v4 => v4,
    };
    let link_local = match host {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    };
    if link_local {
        return Err(format!(
            "refusing to open link-local address {host} (cloud metadata range)"
        ));
    }
    if local_only && !host.is_loopback() {
        return Err(local_only_refusal(url));
    }
    Ok(())
}

fn local_only_refusal(url: &str) -> String {
    format!(
        "refusing to open {url}: [privacy].local_only allows only localhost, \
         127.0.0.1, or [::1]"
    )
}

/// Cut `s` to at most `max` bytes on a char boundary, saying how much went.
#[cfg_attr(not(feature = "browser"), allow(dead_code))] // used by the Chrome path
fn bound(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated: {} more bytes]", &s[..end], s.len() - end)
}

#[async_trait]
impl Tool for Browser {
    /// NETWORK for the obvious reason; WRITE because `screenshot` writes a
    /// file, and because clicking and typing into an app changes its state.
    fn capabilities(&self) -> Capability {
        Capability::NETWORK | Capability::WRITE
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser".into(),
            description: "Drive a headless Chrome tab that persists across calls — for checking \
                 a local dev server. Actions: `navigate` (url) returns title + page text; \
                 `click` / `type` (CSS selector, text) act then return the same; `screenshot` \
                 saves a PNG under .wingman/browser/ and returns its path; `console` returns \
                 console messages and uncaught errors since the last call; `eval` (JS \
                 expression, awaited) returns its JSON. Output is bounded."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["navigate", "click", "type", "screenshot", "console", "eval"] },
                    "url": { "type": "string", "description": "navigate: absolute http(s):// URL." },
                    "selector": { "type": "string", "description": "click/type: CSS selector (waits for it to appear)." },
                    "text": { "type": "string", "description": "type: text to type into the element." },
                    "expression": { "type": "string", "description": "eval: a JS expression, e.g. `document.querySelectorAll('li').length`." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        if !ctx.allows_network() {
            return ToolOutcome::err(
                "network access denied: browser requires auto-edit or yolo mode".to_string(),
            );
        }
        let action = match parse(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(e),
        };
        if let Action::Navigate(url) = &action {
            if let Err(e) = check_url(url, self.local_only) {
                return ToolOutcome::err(e);
            }
        }
        self.drive(action, ctx).await
    }
}

#[cfg(not(feature = "browser"))]
impl Browser {
    async fn drive(&self, _action: Action, _ctx: &ToolCtx) -> ToolOutcome {
        ToolOutcome::err("browser: this build has no `browser` feature")
    }
}

#[cfg(feature = "browser")]
impl Browser {
    async fn drive(&self, action: Action, ctx: &ToolCtx) -> ToolOutcome {
        let session = self.session.clone();
        let local_only = self.local_only;
        // headless_chrome is synchronous; keep it off the reactor. The mutex
        // also serialises concurrent calls onto the one tab.
        let res = tokio::task::spawn_blocking(move || {
            let mut guard = session.lock().unwrap_or_else(|p| p.into_inner());
            if guard.as_ref().is_some_and(|s| !s.alive()) {
                *guard = None; // crashed or disconnected: relaunch below
            }
            if guard.is_none() {
                *guard = Some(wingman_browser::Session::launch(local_only).map_err(|e| {
                    format!("could not start Chrome (`wingman doctor` shows whether one was found): {e}")
                })?);
            }
            let s = guard.as_ref().expect("launched above");
            run_action(s, action, local_only)
        })
        .await;
        match res {
            Ok(Ok(Reply::Text(out))) => ToolOutcome::ok(out),
            Ok(Ok(Reply::Png(url, png))) => save_screenshot(ctx, &url, &png).await,
            Ok(Err(e)) => ToolOutcome::err(e),
            Err(e) => ToolOutcome::err(format!("browser task failed: {e}")),
        }
    }
}

#[cfg(feature = "browser")]
enum Reply {
    Text(String),
    /// The page's URL and its PNG screenshot, saved by the async side through
    /// `ctx.fs`.
    Png(String, Vec<u8>),
}

#[cfg(feature = "browser")]
async fn save_screenshot(ctx: &ToolCtx, url: &str, png: &[u8]) -> ToolOutcome {
    let dir = ctx.project_root.join(".wingman").join("browser");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let path = dir.join(format!("screenshot-{stamp}.png"));
    if !ctx.allows_write(&path) {
        return ToolOutcome::err(ctx.write_denial_reason(&path));
    }
    if let Err(e) = ctx.fs.create_dir_all(&dir).await {
        return ToolOutcome::err(format!("could not save screenshot: {e}"));
    }
    if let Err(e) = ctx.fs.write(&path, png).await {
        return ToolOutcome::err(format!("could not save screenshot: {e}"));
    }
    ToolOutcome::ok(format!(
        "screenshot of {url} saved to {} ({} bytes)",
        path.display(),
        png.len()
    ))
}

#[cfg(feature = "browser")]
fn run_action(
    s: &wingman_browser::Session,
    action: Action,
    local_only: bool,
) -> Result<Reply, String> {
    let e = |e: wingman_browser::BrowserError| e.to_string();
    match action {
        Action::Navigate(url) => s.navigate(&url).map_err(e)?,
        Action::Click(sel) => s.click(&sel).map_err(e)?,
        Action::Type(sel, text) => s.type_into(&sel, &text).map_err(e)?,
        Action::Console => {
            let log = s.drain_console();
            return Ok(Reply::Text(if log.is_empty() {
                "(no console messages since the last call)".into()
            } else {
                crate::wrap_untrusted("browser console", &bound(&log, MAX_EVAL_BYTES))
            }));
        }
        Action::Eval(expr) => {
            guard_current_page(s, local_only)?;
            let out = s.eval_json(&expr).map_err(e)?;
            return Ok(Reply::Text(crate::wrap_untrusted(
                &format!("browser eval {}", s.url()),
                &bound(&out, MAX_EVAL_BYTES),
            )));
        }
        Action::Screenshot => {
            guard_current_page(s, local_only)?;
            return Ok(Reply::Png(s.url(), s.screenshot_png().map_err(e)?));
        }
    }
    // navigate / click / type: report where we ended up.
    guard_current_page(s, local_only)?;
    let (title, text) = s.read().map_err(e)?;
    let url = s.url();
    Ok(Reply::Text(format!(
        "url: {url}\ntitle: {title}\n---\n{}",
        crate::wrap_untrusted(&format!("browser {url}"), &bound(&text, MAX_TEXT_BYTES))
    )))
}

/// A click or a redirect can leave the page the URL check approved. Re-check
/// where the tab actually is before reading anything off it, and blank it if
/// the policy says no.
#[cfg(feature = "browser")]
fn guard_current_page(s: &wingman_browser::Session, local_only: bool) -> Result<(), String> {
    let url = s.url();
    // about:blank, chrome-error:// (a refused connection) and the like carry
    // no remote content.
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Ok(());
    }
    check_url(&url, local_only).map_err(|reason| {
        let _ = s.navigate("about:blank");
        format!("the page moved to a URL the browser may not read: {reason}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_allows_dev_servers_and_public_pages() {
        for u in [
            "http://localhost:3000/",
            "http://127.0.0.1:5173/app",
            "http://[::1]:8080/",
            "http://app.localhost/",
        ] {
            assert!(check_url(u, true).is_ok(), "{u} under local_only");
            assert!(check_url(u, false).is_ok(), "{u}");
        }
        assert!(check_url("https://example.com/", false).is_ok());
        assert!(check_url("http://192.168.1.20:3000/", false).is_ok());
    }

    #[test]
    fn url_policy_refuses_other_schemes_and_metadata() {
        for u in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
            "not a url",
        ] {
            assert!(check_url(u, false).is_err(), "{u}");
        }
    }

    #[test]
    fn local_only_refuses_everything_off_box() {
        for u in [
            "https://example.com/",
            "http://192.168.1.20:3000/",
            "http://10.0.0.1/",
            // Userinfo tricks: the host here is evil.com, not localhost.
            "http://localhost@evil.com/",
            "http://127.0.0.1.evil.com/",
            "http://localhost.evil.com/",
        ] {
            let err = check_url(u, true).unwrap_err();
            assert!(err.contains("local_only"), "{u}: {err}");
        }
    }

    #[test]
    fn bound_cuts_on_a_char_boundary_and_says_so() {
        assert_eq!(bound("short", 10), "short");
        let s = "é".repeat(10); // 20 bytes
        let out = bound(&s, 5);
        assert!(out.starts_with("éé\n"));
        assert!(out.ends_with("[truncated: 16 more bytes]"));
    }

    #[test]
    fn parse_validates_per_action_arguments() {
        assert_eq!(
            parse(json!({"action": "navigate", "url": " http://localhost/ "})),
            Ok(Action::Navigate("http://localhost/".into()))
        );
        assert_eq!(
            parse(json!({"action": "type", "selector": "#q", "text": ""})),
            Ok(Action::Type("#q".into(), String::new()))
        );
        assert_eq!(parse(json!({"action": "console"})), Ok(Action::Console));
        assert!(parse(json!({"action": "navigate"})).is_err());
        assert!(parse(json!({"action": "click", "selector": "  "})).is_err());
        assert!(parse(json!({"action": "type", "selector": "#q"})).is_err());
        assert!(parse(json!({"action": "eval"})).is_err());
        assert!(parse(json!({"action": "scroll"})).is_err());
        assert!(parse(json!({"action": "console", "bogus": 1})).is_err());
    }

    fn ctx(mode: wingman_config::PermissionMode) -> ToolCtx {
        ToolCtx::new(mode, std::env::temp_dir(), std::env::temp_dir())
    }

    #[tokio::test]
    async fn refuses_network_in_read_only() {
        let out = Browser::new(false)
            .run(
                json!({"action": "navigate", "url": "http://localhost/"}),
                &ctx(wingman_config::PermissionMode::ReadOnly),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("network access denied"));
    }

    /// The policy check runs before any browser is launched, so this holds
    /// with or without Chrome installed.
    #[tokio::test]
    async fn local_only_refusal_happens_before_launch() {
        let out = Browser::new(true)
            .run(
                json!({"action": "navigate", "url": "https://example.com/"}),
                &ctx(wingman_config::PermissionMode::Yolo),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("local_only"), "{}", out.content);
    }
}
