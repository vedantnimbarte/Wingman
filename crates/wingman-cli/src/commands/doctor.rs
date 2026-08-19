//! `wingman doctor` — environment & health check.
//!
//! One command that answers "is my Wingman set up correctly?": config,
//! provider credentials, local model servers, the semantic index, language
//! servers on PATH, and the git/gh tooling. Prints a checklist so a user (or a
//! support thread) can see at a glance what's ready and what's missing.

use anyhow::Result;
use std::process::{Command, ExitCode};
use wingman_config::{Config, ProjectPaths};

/// A single health-check line.
enum Status {
    Ok(String),
    Warn(String),
    Bad(String),
}

impl Status {
    fn print(&self) {
        match self {
            Status::Ok(m) => println!("  ✓ {m}"),
            Status::Warn(m) => println!("  ⚠ {m}"),
            Status::Bad(m) => println!("  ✗ {m}"),
        }
    }
}

pub async fn run(cfg: Config) -> Result<ExitCode> {
    let paths = ProjectPaths::discover(&std::env::current_dir()?);
    println!("wingman doctor — {}", paths.root.display());

    let mut bad = 0usize;
    let section = |title: &str| println!("\n{title}:");
    let mut emit = |s: Status| {
        if matches!(s, Status::Bad(_)) {
            bad += 1;
        }
        s.print();
    };

    // 1. Tooling.
    section("tooling");
    emit(bin_status("git", &["--version"]));
    emit(bin_status("gh", &["--version"]));

    // 1b. Shell containment.
    section("shell sandbox");
    {
        let avail = wingman_tools::sandbox::availability();
        let policy = cfg.tools.shell_sandbox.as_str();
        // Three states, not two: a mechanism that scopes the filesystem, one
        // that contains the process but not its file access (Windows), and
        // none. Reporting the middle one as "confined to the project" would
        // be the overclaim this whole section exists to avoid.
        match (policy, avail.is_some(), avail.scopes_filesystem()) {
            ("off", _, _) => emit(Status::Warn(
                "disabled ([tools].shell_sandbox = \"off\") — run_shell is unconfined".into(),
            )),
            (_, _, true) => emit(Status::Ok(format!(
                "{} — run_shell writes are confined to the project",
                avail.label()
            ))),
            ("required", true, false) => emit(Status::Bad(format!(
                "[tools].shell_sandbox = \"required\" needs filesystem scoping; this machine has {} (process containment only), so run_shell will refuse to run",
                avail.label()
            ))),
            (_, true, false) => emit(Status::Warn(format!(
                "{} — no orphaned processes, no clipboard or cross-process handle access, capped process count. Filesystem access is NOT confined: a shell command can still read credentials outside the project (issue #124)",
                avail.label()
            ))),
            ("required", false, _) => emit(Status::Bad(
                "[tools].shell_sandbox = \"required\" but no mechanism is available; run_shell will refuse to run"
                    .into(),
            )),
            (_, false, _) => emit(Status::Warn(
                "no sandbox available — run_shell is unconfined. Install bubblewrap (Linux) for filesystem containment"
                    .into(),
            )),
        }
    }

    // 2. Providers + credentials.
    section("providers");
    if cfg.providers.is_empty() {
        // Blocking, not advisory: with no provider the agent cannot run at
        // all, so reporting "healthy" here was actively misleading.
        emit(Status::Bad(
            "no provider configured — run `wingman login anthropic` \
             (or `wingman discover` for a local model)"
                .into(),
        ));
    }
    for (id, pc) in &cfg.providers {
        let env = provider_env(id);
        let has_key = pc.api_key.as_deref().is_some_and(|k| !k.trim().is_empty())
            || env.is_some_and(|e| std::env::var(e).is_ok());
        let is_local = pc
            .base_url
            .as_deref()
            .is_some_and(|u| u.contains("localhost"));
        if has_key {
            emit(Status::Ok(format!("{id}: credential present")));
        } else if is_local {
            emit(Status::Ok(format!("{id}: local (no key needed)")));
        } else {
            emit(Status::Warn(format!(
                "{id}: no credential ({} unset)",
                env.unwrap_or("api key")
            )));
        }
    }

    // 3. Local model servers.
    section("local model servers");
    for (name, url) in [
        ("ollama", "http://localhost:11434"),
        ("lmstudio", "http://localhost:1234"),
        ("vllm", "http://localhost:8000"),
    ] {
        if tcp_reachable(url) {
            emit(Status::Ok(format!("{name} reachable at {url}")));
        } else {
            emit(Status::Warn(format!("{name} not reachable at {url}")));
        }
    }

    // 4. Semantic index.
    section("semantic index");
    let index = paths.index_db.clone();
    if index.exists() {
        let size = std::fs::metadata(&index).map(|m| m.len()).unwrap_or(0);
        emit(Status::Ok(format!(
            "index present ({} KiB) at {}",
            size / 1024,
            index.display()
        )));
    } else {
        emit(Status::Warn(
            "no index yet — it builds on first TUI run (or `wingman indexd`)".into(),
        ));
    }

    // 5. Language servers on PATH.
    section("language servers (LSP)");
    let mut any_lsp = false;
    for lang in [
        wingman_lsp::Lang::Rust,
        wingman_lsp::Lang::Python,
        wingman_lsp::Lang::TypeScript,
        wingman_lsp::Lang::Go,
        wingman_lsp::Lang::Java,
        wingman_lsp::Lang::C,
        wingman_lsp::Lang::Ruby,
        wingman_lsp::Lang::CSharp,
        wingman_lsp::Lang::Php,
    ] {
        let spec = wingman_lsp::ServerSpec::for_lang(lang);
        match spec.detect() {
            Some((prog, _)) => {
                any_lsp = true;
                emit(Status::Ok(format!("{}: {prog}", lang.label())));
            }
            None => emit(Status::Warn(format!(
                "{}: none on PATH ({})",
                lang.label(),
                spec.candidate_names()
            ))),
        }
    }
    if !any_lsp {
        emit(Status::Warn(
            "no language servers found — lsp_* tools will fall back to tree-sitter".into(),
        ));
    }

    println!();
    if bad == 0 {
        println!("healthy — no blocking problems found.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{bad} problem(s) found (✗). See above.");
        println!("(⚠ lines are optional extras — only ✗ lines block a session.)");
        Ok(ExitCode::from(1))
    }
}

fn bin_status(bin: &str, args: &[&str]) -> Status {
    match Command::new(bin).args(args).output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            let first = v.lines().next().unwrap_or("").trim();
            Status::Ok(format!("{bin}: {first}"))
        }
        _ => Status::Bad(format!("{bin}: not found on PATH")),
    }
}

/// Env var name that holds a provider's key (best-effort for common ones).
fn provider_env(id: &str) -> Option<&'static str> {
    Some(match id {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GOOGLE_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "groq" => "GROQ_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "cohere" => "COHERE_API_KEY",
        "xai" => "XAI_API_KEY",
        _ => return None,
    })
}

/// Cheap reachability probe: can we open a TCP connection to the host:port?
fn tcp_reachable(url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let hostport = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let Ok(mut addrs) = hostport.to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok())
}
