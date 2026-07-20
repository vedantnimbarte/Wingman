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

    println!();
    if local_only && ok {
        println!("ATTESTED: with these settings, code and prompts never leave this machine.");
        println!("(Wingman keeps everything local: memories, index, sessions are on-disk under .wingman/.)");
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
