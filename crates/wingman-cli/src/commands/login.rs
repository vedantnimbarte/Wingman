//! `wingman login` / `wingman logout` — headless credential management.
//!
//! The TUI exposes a `/login` wizard; this is the non-interactive equivalent
//! so keys can be configured from a shell or CI step without entering the
//! alternate screen. It reuses the same building blocks as the wizard:
//! [`crate::login::run_login_task`] for the live probe, the OS keyring via
//! [`wingman_config::secrets`], and the provider table from the TUI modal so
//! the two surfaces stay in lockstep.

use anyhow::{anyhow, Context, Result};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use wingman_config::{ensure_global_dir, global_config_path, secrets, Config};
use wingman_tui::modal::login::PROVIDERS;
use wingman_tui::modal::{LoginPayload, LoginTask};

/// Parsed `wingman login` arguments.
pub struct LoginOptions {
    /// Provider id (e.g. `anthropic`, `openai`, `gemini`). `None` with
    /// `--list`, or to print the available providers.
    pub provider: Option<String>,
    /// API key supplied directly. Takes precedence over the env var.
    pub api_key: Option<String>,
    /// Model id to record as the default for this provider.
    pub model: Option<String>,
    /// Base URL override (for local servers / proxies).
    pub base_url: Option<String>,
    /// Force the browser OAuth flow (chatgpt).
    pub oauth: bool,
    /// Skip the live connectivity test before saving.
    pub no_probe: bool,
    /// Register the provider without making it the default selection.
    pub no_default: bool,
    /// Print the known provider ids and exit.
    pub list: bool,
}

pub async fn run(opts: LoginOptions) -> Result<ExitCode> {
    if opts.list {
        print_providers();
        return Ok(ExitCode::SUCCESS);
    }

    let Some(provider_id) = opts.provider.as_deref() else {
        eprintln!("wingman login: missing PROVIDER.\n");
        print_providers();
        return Ok(ExitCode::from(2));
    };

    let spec = PROVIDERS
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| {
            anyhow!("unknown provider '{provider_id}' — run `wingman login --list` to see options")
        })?;

    let model = opts
        .model
        .clone()
        .unwrap_or_else(|| spec.default_model.to_string());
    let base_url = opts
        .base_url
        .clone()
        .or_else(|| spec.default_base_url.map(str::to_string));

    // Resolve the credential. Three shapes:
    //   - OAuth providers (chatgpt): run the browser flow, tokens land in the
    //     keyring, no api_key travels in the payload.
    //   - key-bearing providers: --api-key, else the conventional env var,
    //     else an interactive prompt on a TTY.
    //   - local providers (no key): nothing to resolve.
    let want_oauth = opts.oauth || spec.needs_oauth;
    let mut api_key: Option<String> = None;

    if want_oauth {
        oauth_login(provider_id).await?;
    } else if spec.needs_key {
        api_key = resolve_key(provider_id, opts.api_key.clone())?;
        if api_key.is_none() {
            let env_hint = crate::runtime::api_key_env_var(provider_id)
                .map(|v| format!(", set ${v},"))
                .unwrap_or_default();
            return Err(anyhow!(
                "{provider_id} requires an API key — pass --api-key{env_hint} or run in a terminal to be prompted"
            ));
        }
    }

    let payload = LoginPayload {
        provider_id: provider_id.to_string(),
        api_key: api_key.clone(),
        base_url: base_url.clone(),
        model: model.clone(),
    };

    // Live probe (reuses the wizard's provider builder), unless skipped.
    if !opts.no_probe {
        eprintln!("Testing {provider_id} ({model})…");
        crate::login::run_login_task(LoginTask::Probe(payload.clone()))
            .await
            .map_err(|e| {
                anyhow!(
                    "probe failed: {e}\n(use --no-probe to save the credential without testing)"
                )
            })?;
        eprintln!("  ✓ provider responded");
    }

    // Persist: key → keyring, then a keyring marker + model/base_url into the
    // global config (promoting to default unless --no-default).
    ensure_global_dir()?;
    if let Some(key) = api_key.as_deref() {
        secrets::store(provider_id, key).context("storing key in keyring")?;
    }
    let with_keyring = api_key.is_some() || secrets::load(provider_id).ok().flatten().is_some();

    let path = global_config_path()?;
    if opts.no_default {
        Config::set_provider_and_save(
            &path,
            provider_id,
            &model,
            base_url.as_deref(),
            with_keyring,
        )
        .context("saving config")?;
    } else {
        Config::set_default_provider_and_save(
            &path,
            provider_id,
            &model,
            base_url.as_deref(),
            with_keyring,
        )
        .context("saving config")?;
    }

    let default_note = if opts.no_default {
        " (not set as default)"
    } else {
        " and set as the default provider"
    };
    println!("Logged in to '{provider_id}' as model '{model}'{default_note}.");
    println!("Config: {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// `wingman logout <provider>` — remove the keyring entry for a provider.
pub async fn logout(provider: String) -> Result<ExitCode> {
    secrets::delete(&provider).context("removing keyring entry")?;
    // ChatGPT stores a second entry for its refresh token.
    if provider == "chatgpt" {
        let _ = secrets::delete("chatgpt_refresh");
    }
    println!("Logged out of '{provider}' (keyring entry removed if present).");
    println!(
        "Note: the [providers.{provider}] section in your config is left in place; \
         remove it manually if you no longer want the provider."
    );
    Ok(ExitCode::SUCCESS)
}

/// Resolve an API key from the explicit flag, then the conventional env var.
/// Falls back to an interactive prompt only when stdin is a terminal.
fn resolve_key(provider_id: &str, from_flag: Option<String>) -> Result<Option<String>> {
    if let Some(k) = from_flag.filter(|k| !k.trim().is_empty()) {
        return Ok(Some(k.trim().to_string()));
    }
    if let Some(env_var) = crate::runtime::api_key_env_var(provider_id) {
        if let Ok(k) = std::env::var(env_var) {
            if !k.trim().is_empty() {
                return Ok(Some(k.trim().to_string()));
            }
        }
    }
    prompt_key(provider_id)
}

/// Prompt for a key on a TTY. Input is echoed (no extra dependency for hidden
/// entry) — prefer `--api-key` or an env var when echo matters.
fn prompt_key(provider_id: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let hint = crate::runtime::api_key_env_var(provider_id)
        .map(|v| format!(" (or Ctrl-C and set ${v})"))
        .unwrap_or_default();
    eprint!("Enter API key for {provider_id}{hint}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading API key from stdin")?;
    let key = line.trim().to_string();
    Ok(if key.is_empty() { None } else { Some(key) })
}

/// Run the ChatGPT OAuth PKCE flow and store both tokens in the keyring.
async fn oauth_login(provider_id: &str) -> Result<()> {
    if provider_id != "chatgpt" {
        return Err(anyhow!(
            "OAuth login is only supported for 'chatgpt'; '{provider_id}' uses an API key"
        ));
    }
    eprintln!("Opening browser for ChatGPT login…");
    let (access, refresh) = crate::oauth::chatgpt_oauth_login()
        .await
        .context("OAuth login failed")?;
    secrets::store("chatgpt", &access).context("storing access token")?;
    secrets::store("chatgpt_refresh", &refresh).context("storing refresh token")?;
    eprintln!("  ✓ authenticated");
    Ok(())
}

fn print_providers() {
    println!("Usage: wingman login <PROVIDER> [--api-key <KEY>] [--model <MODEL>]");
    println!("       wingman login --list");
    println!("       wingman logout <PROVIDER>\n");
    println!("Known providers:");
    for p in PROVIDERS {
        let kind = if p.needs_oauth {
            "oauth"
        } else if p.needs_key {
            "api-key"
        } else {
            "local"
        };
        println!("  {:<14} {:<8} {}", p.id, kind, p.label);
    }
    println!(
        "\nKeys are read from --api-key, the provider's env var, or an interactive prompt,\n\
         and stored in the OS keyring. Local providers need only --base-url."
    );
}
