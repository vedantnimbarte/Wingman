//! `--remote` — run a command against a `wingman serve` daemon instead of
//! locally.
//!
//! Three surfaces, chosen because they are the ones that matter away from the
//! machine doing the work:
//!
//! - `--print` streams a turn's events to stdout, so a prompt runs on the box
//!   with the repo, the index, and the API keys.
//! - `pilot watch` redraws the server's dashboard, so a fleet is watchable
//!   from anywhere.
//! - everything else goes through `/v1/exec` and prints the server's output
//!   verbatim, which means a subcommand added tomorrow works remotely the day
//!   it lands, with no client change.
//!
//! The full interactive TUI is deliberately not remoted: it needs
//! bidirectional permission prompting, which this shape cannot carry, and
//! pretending otherwise would produce a TUI that silently drops approvals.

use std::process::ExitCode;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// Where to reach the server and how to authenticate.
pub struct Remote {
    base: String,
    token: Option<String>,
    client: reqwest::Client,
    /// Project id the server knows this repo by.
    project: String,
}

impl Remote {
    /// Resolve the connection: base URL from `--remote`/`WINGMAN_REMOTE`,
    /// token from `WINGMAN_SERVE_TOKEN` or the OS keyring (the same entry
    /// `wingman serve --init-token` writes, so a client on the serving machine
    /// needs no configuration at all).
    pub async fn connect(base: &str, project: Option<&str>) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let base = if base.contains("://") {
            base
        } else {
            format!("http://{base}")
        };
        let token = std::env::var("WINGMAN_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                wingman_config::secrets::load(crate::serve::auth::KEYRING_ENTRY)
                    .ok()
                    .flatten()
            });
        // Every reqwest client in the workspace installs the process-wide
        // rustls provider first — the crates use `rustls-no-provider`, so
        // building a client without it panics.
        wingman_core::ensure_tls_provider();
        let mut remote = Self {
            base,
            token,
            client: reqwest::Client::new(),
            project: String::new(),
        };
        remote.project = remote.resolve_project(project).await?;
        Ok(remote)
    }

    /// Pick the project to operate on: the one named, else the server's only
    /// one. With several configured and none named, list them rather than
    /// guessing — silently running against the wrong repo is the one failure
    /// mode worth being noisy about.
    async fn resolve_project(&self, requested: Option<&str>) -> Result<String> {
        let listed = self.get_json("/v1/projects").await?;
        let projects = listed["projects"].as_array().cloned().unwrap_or_default();
        let ids: Vec<String> = projects
            .iter()
            .filter_map(|p| p["id"].as_str().map(str::to_string))
            .collect();

        if let Some(want) = requested {
            return if ids.iter().any(|id| id == want) {
                Ok(want.to_string())
            } else {
                Err(anyhow!(
                    "server does not serve a project called '{want}' (it has: {})",
                    ids.join(", ")
                ))
            };
        }
        match ids.len() {
            0 => bail!("the server has no projects configured"),
            1 => Ok(ids[0].clone()),
            _ => bail!(
                "the server serves several projects — pick one with --project (it has: {})",
                ids.join(", ")
            ),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let resp = self
            .auth(self.client.get(&url))
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{status}: {}", short(&body));
        }
        serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
    }

    /// `wingman --remote --print "<prompt>"` — run a turn on the server.
    pub async fn print(
        &self,
        prompt: &str,
        json_out: bool,
        mode: Option<&str>,
    ) -> Result<ExitCode> {
        let mut body = serde_json::json!({ "prompt": prompt });
        if let Some(m) = mode {
            body["mode"] = Value::String(m.to_string());
        }
        let url = format!("{}/v1/projects/{}/turns", self.base, self.project);
        let resp = self
            .auth(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!(
                "{status}: {}",
                short(&resp.text().await.unwrap_or_default())
            );
        }
        self.consume_sse(resp, json_out).await
    }

    /// Read an SSE body and render it. Text deltas go to stdout so the reply
    /// pipes cleanly; tool activity goes to stderr, matching what local
    /// `--print` does.
    async fn consume_sse(&self, mut resp: reqwest::Response, json_out: bool) -> Result<ExitCode> {
        use std::io::Write;
        let mut buf = String::new();
        let mut exit = ExitCode::SUCCESS;
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();

        while let Some(chunk) = resp.chunk().await? {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // SSE frames are separated by a blank line; keep the tail, which
            // may be a frame still arriving.
            while let Some(idx) = buf.find("\n\n") {
                let frame = buf[..idx].to_string();
                buf.drain(..idx + 2);
                let Some(data) = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .map(str::to_string)
                else {
                    continue; // a keepalive comment or a frame with no data
                };
                if json_out {
                    println!("{data}");
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                match event["type"].as_str() {
                    Some("text_delta") => {
                        write!(stdout, "{}", event["text"].as_str().unwrap_or_default()).ok();
                        stdout.flush().ok();
                    }
                    Some("tool_start") => {
                        eprintln!("\n[tool] {}…", event["name"].as_str().unwrap_or("?"));
                    }
                    Some("verification") => {
                        let mark = if event["passed"].as_bool() == Some(true) {
                            "✓"
                        } else {
                            "✗"
                        };
                        eprintln!(
                            "\n[verify {mark}] {}",
                            event["summary"].as_str().unwrap_or("")
                        );
                    }
                    Some("error") => {
                        eprintln!("\n[error] {}", event["message"].as_str().unwrap_or(""));
                        exit = ExitCode::from(1);
                    }
                    _ => {
                        // The terminal `end` frame carries the child's exit
                        // code; anything non-zero must fail this process too,
                        // or a remote CI step would report green on a red run.
                        if let Some(code) = event["exit"].as_i64() {
                            writeln!(stdout).ok();
                            if code != 0 {
                                if let Some(err) = event["stderr"].as_str() {
                                    eprint!("{err}");
                                }
                                exit = ExitCode::from(code.clamp(1, 255) as u8);
                            }
                        }
                    }
                }
            }
        }
        Ok(exit)
    }

    /// `wingman --remote pilot watch [run]` — redraw the server's dashboard.
    ///
    /// Polls the ASCII dashboard endpoint rather than reimplementing the TUI
    /// against remote state: it is the same `render_dashboard` output the
    /// local watcher draws, so the two cannot diverge.
    pub async fn watch(&self, run_id: Option<&str>) -> Result<ExitCode> {
        let run_id = match run_id {
            Some(id) => id.to_string(),
            None => {
                let runs = self
                    .get_json(&format!("/v1/projects/{}/pilot/runs", self.project))
                    .await?;
                runs["runs"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(|r| r["run_id"].as_str())
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("no runs on the server for '{}'", self.project))?
            }
        };
        let width = 100;
        let url = format!(
            "{}/v1/projects/{}/pilot/runs/{run_id}/dashboard?width={width}",
            self.base, self.project
        );
        loop {
            let resp = self.auth(self.client.get(&url)).send().await?;
            if !resp.status().is_success() {
                bail!(
                    "{}: {}",
                    resp.status(),
                    short(&resp.text().await.unwrap_or_default())
                );
            }
            let text = resp.text().await.unwrap_or_default();
            // Clear and home, so successive frames overwrite rather than
            // scroll. No alternate screen: Ctrl-C should leave the last frame
            // on screen the way `pilot status` would.
            print!("\x1b[2J\x1b[H{text}");
            use std::io::Write;
            std::io::stdout().flush().ok();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Anything else: hand the argv to `/v1/exec` and print what comes back.
    pub async fn exec(&self, args: &[String]) -> Result<ExitCode> {
        let url = format!("{}/v1/projects/{}/exec", self.base, self.project);
        let resp = self
            .auth(self.client.post(&url))
            .json(&serde_json::json!({ "args": args }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

        if !status.is_success() {
            let msg = parsed["error"].as_str().unwrap_or(&body);
            bail!("{status}: {msg}");
        }
        // A table route answering with the command's own JSON, versus the
        // {stdout,stderr,exit} envelope for text commands.
        match parsed["stdout"].as_str() {
            Some(out) => {
                print!("{out}");
                if let Some(err) = parsed["stderr"].as_str() {
                    eprint!("{err}");
                }
                let code = parsed["exit"].as_i64().unwrap_or(0);
                Ok(ExitCode::from(code.clamp(0, 255) as u8))
            }
            None => {
                println!("{}", serde_json::to_string_pretty(&parsed).unwrap_or(body));
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}

fn short(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= 300 {
        return trimmed.to_string();
    }
    format!("{}…", &trimmed[..300])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_port_gets_a_scheme() {
        // Constructed the same way `connect` does, without a live server.
        let base = "box:8787";
        let normalised = if base.contains("://") {
            base.to_string()
        } else {
            format!("http://{base}")
        };
        assert_eq!(normalised, "http://box:8787");
    }

    #[test]
    fn long_error_bodies_are_truncated() {
        let long = "x".repeat(1000);
        assert!(short(&long).len() < 320);
        assert_eq!(short("  brief  "), "brief");
    }
}
