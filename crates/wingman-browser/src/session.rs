//! A long-lived headless browser the agent drives across tool calls.
//!
//! [`capture`](crate::capture) is one-shot: launch, load, screenshot, exit.
//! The agent's `browser` tool needs the opposite — one tab that survives
//! between calls so a click lands on the page the previous call loaded. That
//! is [`Session`]: launched lazily by the tool, killed when dropped
//! (`headless_chrome` kills the Chrome process in its own `Drop`).
//!
//! Everything policy-shaped (which URLs are allowed, argument validation,
//! output bounds) lives in the tool, not here; this is only the driver.

use std::collections::VecDeque;

/// Most console lines retained between two `console` reads.
///
/// A page that logs in a loop would otherwise grow this for the whole
/// session. The oldest lines go first and are counted, so a reader is told
/// what it missed instead of being shown a partial log as if it were whole.
pub const MAX_CONSOLE_LINES: usize = 200;

/// Console messages and uncaught errors seen since the last [`drain`](Self::drain).
#[derive(Debug, Default)]
pub struct ConsoleBuffer {
    lines: VecDeque<String>,
    dropped: usize,
}

impl ConsoleBuffer {
    pub fn push(&mut self, line: String) {
        if self.lines.len() == MAX_CONSOLE_LINES {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// Everything buffered, oldest first, and reset.
    pub fn drain(&mut self) -> String {
        let mut out = String::new();
        if self.dropped > 0 {
            out.push_str(&format!(
                "[{} older message(s) dropped]\n",
                std::mem::take(&mut self.dropped)
            ));
        }
        for line in self.lines.drain(..) {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

/// Path of the Chrome/Chromium binary a [`Session`] would launch, for
/// `wingman doctor`.
#[cfg(feature = "chrome")]
pub fn find_chrome() -> Result<std::path::PathBuf, String> {
    headless_chrome::browser::default_executable()
}

#[cfg(not(feature = "chrome"))]
pub fn find_chrome() -> Result<std::path::PathBuf, String> {
    Err("built without the `browser` feature".into())
}

#[cfg(feature = "chrome")]
pub use chrome::Session;

#[cfg(feature = "chrome")]
mod chrome {
    use super::ConsoleBuffer;
    use crate::{BrowserError, Result};
    use headless_chrome::protocol::cdp::types::Event;
    use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
    use headless_chrome::protocol::cdp::Runtime::RemoteObject;
    use headless_chrome::{Browser, LaunchOptions, Tab};
    use std::sync::{Arc, Mutex};

    fn err(e: impl std::fmt::Display) -> BrowserError {
        BrowserError::Browser(e.to_string())
    }

    /// A primitive's value, else the object's description (`Error: boom`).
    fn render(o: &RemoteObject) -> String {
        match &o.value {
            Some(v) => v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string()),
            None => o.description.clone().unwrap_or_default(),
        }
    }

    pub struct Session {
        // Held for its `Drop`, which kills Chrome.
        _browser: Browser,
        tab: Arc<Tab>,
        console: Arc<Mutex<ConsoleBuffer>>,
    }

    impl Session {
        /// Launch headless Chrome with one tab.
        ///
        /// `local_only` points Chrome at a dead proxy. Chrome implicitly
        /// bypasses proxies for loopback, so `localhost` dev servers still
        /// load while every other request — a CDN script, an analytics
        /// beacon, a redirect off-box — fails. The tool checks the URL it is
        /// asked to open; this catches what the page itself fetches.
        /// ponytail: WebRTC can ignore proxy settings; a page opening peer
        /// connections under local_only is not blocked.
        pub fn launch(local_only: bool) -> Result<Self> {
            let opts = LaunchOptions::default_builder()
                .headless(true)
                // The default 30s idle timeout drops the connection whenever
                // the agent spends half a minute thinking between calls.
                .idle_browser_timeout(std::time::Duration::from_secs(24 * 60 * 60))
                .proxy_server(local_only.then_some("127.0.0.1:9"))
                .build()
                .map_err(err)?;
            let browser = Browser::new(opts).map_err(err)?;
            let tab = browser.new_tab().map_err(err)?;
            tab.enable_log().map_err(err)?;
            tab.enable_runtime().map_err(err)?;

            let console = Arc::new(Mutex::new(ConsoleBuffer::default()));
            let sink = console.clone();
            tab.add_event_listener(Arc::new(move |event: &Event| {
                let line = match event {
                    Event::RuntimeConsoleAPICalled(e) => {
                        let args: Vec<String> = e.params.args.iter().map(render).collect();
                        format!(
                            "console.{}: {}",
                            format!("{:?}", e.params.Type).to_lowercase(),
                            args.join(" ")
                        )
                    }
                    Event::RuntimeExceptionThrown(e) => {
                        let d = &e.params.exception_details;
                        let detail = d.exception.as_ref().map(render).unwrap_or_default();
                        format!("uncaught: {} {detail}", d.text)
                    }
                    // Browser-side entries: failed requests, CSP violations.
                    Event::LogEntryAdded(e) => format!(
                        "log.{}: {}",
                        format!("{:?}", e.params.entry.level).to_lowercase(),
                        e.params.entry.text
                    ),
                    _ => return,
                };
                sink.lock().unwrap_or_else(|p| p.into_inner()).push(line);
            }))
            .map_err(err)?;

            Ok(Self {
                _browser: browser,
                tab,
                console,
            })
        }

        pub fn navigate(&self, url: &str) -> Result<()> {
            self.tab.navigate_to(url).map_err(err)?;
            self.tab.wait_until_navigated().map_err(err)?;
            Ok(())
        }

        pub fn click(&self, selector: &str) -> Result<()> {
            self.tab
                .wait_for_element(selector)
                .map_err(err)?
                .click()
                .map_err(err)?;
            self.settle();
            Ok(())
        }

        pub fn type_into(&self, selector: &str, text: &str) -> Result<()> {
            self.tab
                .wait_for_element(selector)
                .map_err(err)?
                .type_into(text)
                .map_err(err)?;
            self.settle();
            Ok(())
        }

        /// A click can start a navigation; wait for it rather than reading
        /// the page it is leaving. `wait_until_navigated` returns at once
        /// when nothing is loading.
        fn settle(&self) {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = self.tab.wait_until_navigated();
        }

        pub fn url(&self) -> String {
            self.tab.get_url()
        }

        /// Title and `innerText` of the current page.
        pub fn read(&self) -> Result<(String, String)> {
            let title = self.eval_string("document.title")?;
            let text = self.eval_string("document.body ? document.body.innerText : ''")?;
            Ok((title, text))
        }

        pub fn screenshot_png(&self) -> Result<Vec<u8>> {
            self.tab
                .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                .map_err(err)
        }

        /// Evaluate a JS expression (awaited if it is a promise) and return
        /// its `JSON.stringify` form. A throw comes back as an error.
        pub fn eval_json(&self, expression: &str) -> Result<String> {
            // The newline before `)` keeps a trailing `// comment` in the
            // expression from swallowing the closing paren.
            let wrapped = format!(
                "(async () => {{ const v = await ({expression}\n); \
                 try {{ return JSON.stringify(v) ?? 'undefined'; }} \
                 catch (e) {{ return String(v); }} }})()"
            );
            self.eval_string(&wrapped)
        }

        fn eval_string(&self, expression: &str) -> Result<String> {
            let o = self.tab.evaluate(expression, true).map_err(err)?;
            match &o.value {
                Some(v) if v.is_string() => Ok(render(&o)),
                // Evaluation threw (or rejected): the result is the error.
                _ => Err(BrowserError::Browser(format!(
                    "evaluation failed: {}",
                    render(&o)
                ))),
            }
        }

        pub fn drain_console(&self) -> String {
            self.console
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .drain()
        }

        /// Whether Chrome still answers. A crashed tab or a dead connection
        /// fails every later call, so the tool relaunches instead.
        pub fn alive(&self) -> bool {
            self.tab.evaluate("1", false).is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_buffer_drains_in_order_and_resets() {
        let mut b = ConsoleBuffer::default();
        b.push("one".into());
        b.push("two".into());
        assert_eq!(b.drain(), "one\ntwo\n");
        assert_eq!(b.drain(), "");
    }

    #[test]
    fn console_buffer_is_bounded_and_reports_what_it_dropped() {
        let mut b = ConsoleBuffer::default();
        for i in 0..MAX_CONSOLE_LINES + 3 {
            b.push(format!("line {i}"));
        }
        let out = b.drain();
        assert!(out.starts_with("[3 older message(s) dropped]\n"));
        assert!(!out.contains("line 2\n"));
        assert!(out.contains("line 3\n"));
        assert!(out.ends_with(&format!("line {}\n", MAX_CONSOLE_LINES + 2)));
        assert_eq!(b.drain(), "", "the dropped count resets too");
    }
}
