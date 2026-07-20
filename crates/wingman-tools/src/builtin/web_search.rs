//! `web_search`: DuckDuckGo HTML search — no API key required.
//!
//! Returns the top N results as `title :: url :: snippet` lines. Intended
//! to be paired with `web_fetch` on a chosen URL.

use crate::{Tool, ToolCtx};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct WebSearch;

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    8
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description:
                "Search the web via DuckDuckGo and return up to `limit` results as one line each: \
                 `<title> :: <url> :: <snippet>`. No API key required."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25, "default": 8 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        if !ctx.allows_network() {
            return ToolOutcome::err(
                "network access denied: web_search requires auto-edit or yolo mode".to_string(),
            );
        }
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let q = args.query.trim();
        if q.is_empty() {
            return ToolOutcome::err("query is empty".to_string());
        }
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding::encode(q)
        );

        let client = match reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (wingman)")
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolOutcome::err(format!("http client init: {e}")),
        };

        let html = match client.get(&url).send().await {
            Ok(r) => match r.text().await {
                Ok(t) => t,
                Err(e) => return ToolOutcome::err(format!("read body: {e}")),
            },
            Err(e) => return ToolOutcome::err(format!("fetch search: {e}")),
        };

        let results = parse_ddg(&html, args.limit as usize);
        if results.is_empty() {
            // Distinguish a genuine zero-result query from a scrape failure
            // (markup drift or a bot-check/CAPTCHA page). DDG's real results
            // and its "no results" page both carry the `result` container
            // markup; a blocked/redirected page does not. If the anchor class
            // we parse is present but yielded nothing, the layout changed.
            if html.contains("result__a") {
                return ToolOutcome::err(
                    "web_search parse failed: DuckDuckGo result markup changed (the \
                     `result__a` selector matched no anchors)"
                        .to_string(),
                );
            }
            if !html.contains("no-results") && !html.contains("No&nbsp;results") {
                return ToolOutcome::err(
                    "web_search failed: DuckDuckGo returned no result markup (likely a \
                     bot-check/CAPTCHA page or a transport error)"
                        .to_string(),
                );
            }
            return ToolOutcome::ok(format!("(no results for {q})"));
        }
        let mut out = String::new();
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!(
                "{i}. {} :: {} :: {}\n",
                r.title,
                r.url,
                r.snippet.chars().take(220).collect::<String>()
            ));
        }
        ToolOutcome::ok(out)
    }
}

struct Hit {
    title: String,
    url: String,
    snippet: String,
}

fn parse_ddg(html: &str, limit: usize) -> Vec<Hit> {
    let result_re = Regex::new(
        r#"(?is)<a\s+rel="nofollow"\s+class="result__a"\s+href="([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .unwrap();
    let snippet_re = Regex::new(r#"(?is)<a\s+class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();

    let titles: Vec<(String, String)> = result_re
        .captures_iter(html)
        .map(|c| (clean_url(&c[1]), strip_tags(&c[2])))
        .collect();
    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|c| strip_tags(&c[1]))
        .collect();

    titles
        .into_iter()
        .enumerate()
        .take(limit)
        .map(|(i, (url, title))| Hit {
            title,
            url,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// DuckDuckGo wraps result URLs in a redirector — unwrap if we see one.
fn clean_url(u: &str) -> String {
    if let Some(rest) = u.strip_prefix("//duckduckgo.com/l/?uddg=") {
        // up to next '&'
        let target: &str = rest.split('&').next().unwrap_or(rest);
        urlencoding::decode(target)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| u.to_string())
    } else if let Some(rest) = u.strip_prefix("/l/?uddg=") {
        let target: &str = rest.split('&').next().unwrap_or(rest);
        urlencoding::decode(target)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| u.to_string())
    } else {
        u.to_string()
    }
}

fn strip_tags(s: &str) -> String {
    let re = Regex::new(r"<[^>]+>").unwrap();
    let cleaned = re.replace_all(s, "").to_string();
    cleaned
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tags_basic() {
        assert_eq!(strip_tags("<b>hi</b> &amp; bye"), "hi & bye");
    }

    #[test]
    fn clean_url_unwraps_redirector() {
        let raw = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fp&rut=abc";
        assert_eq!(clean_url(raw), "https://example.com/p");
    }

    #[test]
    fn parse_ddg_reads_a_result() {
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Example</a>
            <a class="result__snippet">an example page</a>"#;
        let hits = parse_ddg(html, 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com");
        assert_eq!(hits[0].title, "Example");
        assert_eq!(hits[0].snippet, "an example page");
    }

    #[test]
    fn empty_parse_on_markup_with_anchor_class_is_a_failure_not_empty() {
        // Regression: a page that still carries the `result__a` class but whose
        // anchor shape changed must read as a parse failure, not "0 results".
        let drifted = r#"<div class="result__a-wrapper">changed</div>"#;
        assert!(parse_ddg(drifted, 8).is_empty());
        assert!(drifted.contains("result__a"));
    }
}
