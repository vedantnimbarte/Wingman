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

pub async fn run(cfg: Config, fix: bool, lint: bool, json: bool) -> Result<ExitCode> {
    let paths = ProjectPaths::discover(&std::env::current_dir()?);

    // Config comes first, and not only for tidiness: a layer that fails to
    // parse means `cfg` is defaults, and every check below it would then be
    // answering questions about a configuration that is not in force.
    let config_reports = check_config(&paths, fix, json)?;
    let config_bad: usize = config_reports.iter().map(|r| r.findings.len()).sum();

    if lint {
        // Read-only, no probes, no network, no PATH walk. The point is a
        // preflight that is fast enough to put in front of every CI job.
        return Ok(if config_bad == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }

    println!("wingman doctor — {}", paths.root.display());

    let mut bad = config_bad;
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
    // Optional: only `notebook_run` needs it, so missing is a warning.
    emit(match bin_status("jupyter", &["nbconvert", "--version"]) {
        Status::Ok(v) => Status::Ok(format!("{v} (nbconvert) — notebook_run available")),
        _ => Status::Warn(
            "jupyter nbconvert: not found on PATH — notebook_run needs it \
             (`pip install nbconvert ipykernel`)"
                .into(),
        ),
    });

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

    // 1c. Pilot sandbox tiers (J11). A missing backend is a warning, not a
    // failure: most machines have neither, and pilot degrades or refuses
    // accordingly.
    section("pilot sandbox tiers");
    {
        let sandbox = &cfg.pilot.sandbox;
        let avail = wingman_autonomous::sandbox::TierAvailability::probe(
            sandbox,
            &wingman_autonomous::pr::SystemCommandRunner,
        );
        emit(Status::Ok("host — always available".into()));
        emit(if avail.docker {
            Status::Ok(format!(
                "container — Docker daemon reachable; workers run in {} (unvalidated against a real daemon)",
                sandbox.container_image
            ))
        } else {
            Status::Warn(
                "container — no Docker daemon reachable; container-tier tasks run on the host"
                    .into(),
            )
        });
        emit(match &avail.vm {
            Ok(()) => {
                Status::Ok("vm — Firecracker ready (unvalidated against a real KVM host)".into())
            }
            Err(why) if sandbox.allow_unsandboxed_vm_tasks => Status::Warn(format!(
                "vm — unavailable ({why}); allow_unsandboxed_vm_tasks lets vm-tier tasks run with weaker isolation"
            )),
            Err(why) => Status::Warn(format!(
                "vm — unavailable ({why}); pilot refuses vm-tier tasks"
            )),
        });
        // `bg start --devcontainer` runs through the same Docker probe.
        let devcontainer = paths.root.join(".devcontainer").join("devcontainer.json");
        if devcontainer.exists() {
            emit(
                match (avail.docker, super::bg::devcontainer_spec(&paths.root)) {
                    (true, Ok(spec)) => Status::Ok(format!(
                        "bg --devcontainer — {spec} (unvalidated against a real daemon)"
                    )),
                    (false, Ok(_)) => Status::Warn(
                        "bg --devcontainer — no Docker daemon reachable; it will refuse to start"
                            .into(),
                    ),
                    (_, Err(e)) => Status::Warn(format!("bg --devcontainer — {e:#}")),
                },
            );
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

    // 2b. Reasoning. Only interesting when it is switched on: the failure
    // mode worth catching is a level that is configured but silently dropped
    // because the active backend has no reasoning control.
    {
        let level = cfg.reasoning.trim();
        if !matches!(level, "" | "off" | "none" | "false") {
            section("reasoning");
            let provider = cfg.default_provider.as_deref().unwrap_or("");
            // Named rather than probed: `capabilities()` needs a constructed
            // provider, and doctor deliberately does not build one.
            const HAS_REASONING: &[&str] = &["anthropic", "openai", "gemini", "openrouter"];
            if provider.is_empty() {
                emit(Status::Warn(format!(
                    "reasoning = \"{level}\" but no default_provider is set"
                )));
            } else if HAS_REASONING.contains(&provider) {
                emit(Status::Ok(format!(
                    "reasoning = \"{level}\" — {provider} supports it (thinking tokens bill at the output rate)"
                )));
            } else {
                emit(Status::Warn(format!(
                    "reasoning = \"{level}\" but {provider} has no reasoning control, so it is ignored"
                )));
            }
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
            "no index yet — it builds on first TUI run (or `wingman indexd start`)".into(),
        ));
    }
    match crate::commands::indexd::live_pid(&paths.dir) {
        Some(pid) => emit(Status::Ok(format!(
            "indexd running (pid {pid}) — sessions open with its warm index"
        ))),
        None => emit(Status::Warn(
            "indexd not running — `wingman indexd start` keeps the index warm between sessions"
                .into(),
        )),
    }

    // 4b. OTLP export. A TCP probe only: it says a collector is listening,
    // not that it accepts OTLP/HTTP JSON or these headers.
    section("telemetry (OTLP)");
    match wingman_session::otlp::settings(&cfg, |k| std::env::var(k).ok()) {
        Ok(None) => emit(Status::Ok("not configured — nothing is exported".into())),
        Err(e) => emit(Status::Bad(format!("refused, export is off: {e}"))),
        Ok(Some(s)) => {
            let shown = wingman_session::otlp::display_endpoint(&s.endpoint);
            let hostport = reqwest::Url::parse(&s.endpoint)
                .ok()
                .and_then(|u| Some(format!("{}:{}", u.host_str()?, u.port_or_known_default()?)));
            if hostport.as_deref().is_some_and(tcp_reachable) {
                emit(Status::Ok(format!("exporting to {shown} (reachable)")));
            } else {
                emit(Status::Warn(format!(
                    "exporting to {shown}, but it is not reachable — spans will be dropped"
                )));
            }
        }
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

    // 6. Debug adapters on PATH, for the debug_* tools.
    section("debug adapters (DAP)");
    for lang in wingman_tools::dap::DebugLang::ALL {
        match wingman_tools::dap::Adapter::detect(lang) {
            Some(adapter) => emit(Status::Ok(format!("{}: {}", lang.label(), adapter.program))),
            None => emit(Status::Warn(format!(
                "{}: none on PATH (install {})",
                lang.label(),
                lang.install_hint()
            ))),
        }
    }

    // 7. Headless browser (verify gate + the `browser` tool).
    section("browser");
    if cfg!(feature = "browser") {
        match wingman_browser::find_chrome() {
            Ok(path) => emit(Status::Ok(format!("Chrome/Chromium: {}", path.display()))),
            Err(e) => emit(Status::Warn(format!(
                "no Chrome/Chromium found ({e}) — the `browser` tool and [verify.browser] gate will not run; set CHROME to its path"
            ))),
        }
    } else {
        emit(Status::Warn(
            "built without the `browser` feature — no `browser` tool or visual verification".into(),
        ));
    }

    // Claude Code hooks are never imported silently, so the only way to
    // discover the option is to be told it applies to you.
    if !cfg.hooks.import_claude_code {
        let candidates = [
            wingman_config::user_home()
                .ok()
                .map(|h| h.join(".claude").join("settings.json")),
            Some(paths.root.join(".claude").join("settings.json")),
        ];
        let found: Vec<String> = candidates
            .into_iter()
            .flatten()
            .filter(|p| p.exists() && file_declares_hooks(p))
            .map(|p| p.display().to_string())
            .collect();
        if !found.is_empty() {
            section("claude code");
            emit(Status::Warn(format!(
                "found hooks in {} — set [hooks].import_claude_code = true to run them \
                 here instead of rewriting them (a project file also needs `wingman trust`)",
                found.join(", ")
            )));
        }
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

/// Parse every config layer that exists, report what is wrong with it, and —
/// under `--fix` — repair what can be repaired unambiguously.
///
/// Returns one report per file that exists, whether or not it had findings, so
/// the JSON output says which layers were actually inspected rather than
/// leaving the caller to guess.
fn check_config(
    paths: &ProjectPaths,
    fix: bool,
    json: bool,
) -> Result<Vec<super::doctor_repair::FileReport>> {
    use super::doctor_repair;

    let layers = [
        wingman_config::global_config_path().ok(),
        Some(paths.config_file.clone()),
    ];
    let mut reports = Vec::new();
    for path in layers.into_iter().flatten() {
        if !path.exists() {
            continue;
        }
        match doctor_repair::analyze(&path) {
            Ok(r) => reports.push(r),
            // An unreadable config is a real problem, but it is a permissions
            // or IO problem and not one a rename can fix. Say so and move on.
            Err(e) => {
                if !json {
                    Status::Bad(format!("{}: cannot read — {e}", path.display())).print();
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        // `--fix` still applies under `--json`; the report above describes
        // what was found, and the writes below are reported by their absence
        // from the next run.
    } else {
        println!("config:");
        if reports.is_empty() {
            Status::Warn("no config file — run `wingman config init`".into()).print();
        }
        for r in &reports {
            if r.is_clean() {
                Status::Ok(format!("{}", r.path.display())).print();
            } else {
                Status::Bad(format!("{}", r.path.display())).print();
                for f in &r.findings {
                    println!("      {}", f.describe());
                }
            }
        }
    }

    if fix {
        for r in &reports {
            let Some(text) = &r.repaired else {
                // Either clean, or something in it needs a person. Both are
                // "nothing to write", and the findings above already said which.
                continue;
            };
            match doctor_repair::write_repaired(&r.path, text) {
                Ok(backup) => println!(
                    "  ✓ repaired {} ({} key(s)); previous version kept at {}",
                    r.path.display(),
                    r.findings.len(),
                    backup.display()
                ),
                Err(e) => println!("  ✗ could not write {}: {e}", r.path.display()),
            }
        }
    } else if reports.iter().any(|r| r.repaired.is_some()) {
        println!("  → `wingman doctor --fix` can repair these (the file is backed up first)");
    }

    Ok(reports)
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

/// Whether a Claude Code settings file actually declares any hooks.
///
/// Most `settings.json` files have none, and warning about a file that would
/// import nothing is noise — the kind that teaches people to ignore warnings.
fn file_declares_hooks(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("hooks")
                .and_then(|h| h.as_object().map(|o| !o.is_empty()))
        })
        .unwrap_or(false)
}
