//! `wingman attest` — report the air-gapped / local-only guarantees.
//!
//! For regulated or air-gapped teams: a single command that states, and
//! verifies against config, what leaves the machine. Prints ✓/✗ so it can be
//! captured for a compliance record.

use anyhow::Result;
use std::process::ExitCode;
use wingman_config::Config;

pub async fn run(cfg: Config) -> Result<ExitCode> {
    println!("wingman attestation — local-only / air-gapped posture\n");

    let local_only = cfg.privacy.local_only;
    let mut ok = true;
    let mut line = |pass: bool, msg: &str| {
        if !pass {
            ok = false;
        }
        println!("  {} {msg}", if pass { "✓" } else { "✗" });
    };

    line(local_only, "[privacy].local_only is enabled");

    // Default provider is local.
    if let Some(p) = &cfg.default_provider {
        let is_local = crate::runtime::provider_is_local(&cfg, p);
        line(is_local, &format!("default provider '{p}' is local"));
    } else {
        line(false, "no default provider configured");
    }

    // Network tools are gated (they're removed at runtime under local_only).
    line(
        local_only,
        "web_fetch / web_search disabled (network tools off)",
    );

    // Not routing any task class to a non-local model.
    let bad_class: Option<String> = cfg
        .router
        .classes
        .values()
        .chain(cfg.router.fast_model.iter())
        .chain(cfg.router.local_model.iter())
        .find(|m| {
            let prov = m.split('/').next().unwrap_or("");
            !prov.is_empty()
                && prov != "fast"
                && prov != "local"
                && prov != "default"
                && !crate::runtime::provider_is_local(&cfg, prov)
        })
        .cloned();
    match bad_class {
        Some(m) => line(false, &format!("router targets a non-local model: {m}")),
        None => line(true, "no router class targets a non-local model"),
    }

    // The checks above only ever looked at the two network *tools* and the
    // provider. Everything below is an egress channel that `local_only` does
    // not disable — attesting without inspecting them meant the command could
    // print a clean compliance record while data was actively leaving.

    // MCP servers with an HTTP transport talk to the network directly; stdio
    // servers are arbitrary local binaries that can do the same.
    let http_mcp: Vec<&String> = cfg
        .mcp
        .iter()
        .filter(|(_, s)| s.url.is_some())
        .map(|(n, _)| n)
        .collect();
    let stdio_mcp: Vec<&String> = cfg
        .mcp
        .iter()
        .filter(|(_, s)| {
            s.url.is_none() && s.command.as_deref().is_some_and(|c| !c.trim().is_empty())
        })
        .map(|(n, _)| n)
        .collect();
    match (http_mcp.is_empty(), stdio_mcp.is_empty()) {
        (true, true) => line(true, "no MCP servers configured"),
        (false, _) => line(
            false,
            &format!(
                "MCP server(s) with an HTTP transport can reach the network: {}",
                http_mcp
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        (true, false) => line(
            false,
            &format!(
                "MCP server(s) run local binaries this command cannot vouch for: {}",
                stdio_mcp
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    }

    // Hooks and custom tools are arbitrary shell — curl, git push, anything.
    let hook_count = cfg.hooks.pre_tool_use.len()
        + cfg.hooks.post_tool_use.len()
        + cfg.hooks.stop.len()
        + cfg.hooks.user_prompt_submit.len();
    line(
        hook_count == 0,
        &format!("no [hooks] configured (found {hook_count}; hooks run arbitrary shell)"),
    );
    line(
        cfg.tools.custom.is_empty(),
        &format!(
            "no [[tools.custom]] configured (found {}; they run arbitrary shell)",
            cfg.tools.custom.len()
        ),
    );

    // Team memory push uploads project memories to a remote endpoint.
    line(
        cfg.team.endpoint.is_none(),
        "no [team].endpoint configured (memory push/pull would leave the machine)",
    );

    // run_shell is the honest caveat: it is available in edit modes and can
    // reach the network however the OS allows. Say so rather than implying a
    // guarantee the tool cannot make.
    let shell_disabled = cfg.tools.disabled_tools.iter().any(|t| t == "run_shell");
    let shell_mode = matches!(
        cfg.permission_mode,
        wingman_config::PermissionMode::AutoEdit | wingman_config::PermissionMode::Yolo
    );
    if shell_disabled {
        line(true, "`run_shell` is disabled via [tools].disabled_tools");
    } else if shell_mode {
        line(
            false,
            &format!(
                "`run_shell` is reachable in {} mode: the agent can run arbitrary \
                 commands, which no config setting can contain",
                cfg.permission_mode
            ),
        );
    } else {
        line(
            true,
            &format!(
                "`run_shell` is not reachable in {} mode",
                cfg.permission_mode
            ),
        );
    }

    println!();
    if local_only && ok {
        println!("ATTESTED: with these settings, no configured Wingman channel sends");
        println!("code or prompts off this machine.");
        println!();
        println!("Scope of this attestation: it reflects configuration only. It does not");
        println!("and cannot vouch for what a local model, a local MCP binary, or any");
        println!("process the agent starts does with the data once it has it, nor for");
        println!("network access outside Wingman. Memories, index, and sessions are");
        println!("on-disk under .wingman/.");
        Ok(ExitCode::SUCCESS)
    } else if !local_only {
        println!(
            "NOT air-gapped: set `[privacy].local_only = true` and use a local provider to attest."
        );
        Ok(ExitCode::from(1))
    } else {
        println!("local_only is on but the checks above found a gap — fix the ✗ lines.");
        Ok(ExitCode::from(1))
    }
}
