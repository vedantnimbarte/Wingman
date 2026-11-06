//! `/v1/providers` — connect, test and disconnect model providers from the
//! web panel.
//!
//! The browser twin of `wingman login` / `wingman logout`, built from the same
//! parts so the two are interchangeable: the provider table is the TUI
//! wizard's [`PROVIDERS`], the live test is [`crate::login::run_login_task`],
//! and a key lands in the OS keyring with only a `keyring:<id>` marker in
//! `config.toml` — exactly where the CLI puts it. A key saved here works from
//! the terminal, and one saved from the terminal shows up here as connected.
//!
//! **Keys are write-only.** No route returns a key, or any part of one. A read
//! says *where* a credential comes from (keychain, environment, config file),
//! never what it is — the same rule `GET /v1/config` keeps with redaction.
//!
//! Every route sits behind the same authentication as the rest of `/v1`.
//! Config writes go through [`super::admin::edit_global`], so they share the
//! comment-preserving editor, the round-trip validation and the atomic write
//! that `PATCH /v1/config` uses.

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use wingman_config::secrets;
use wingman_tui::modal::login::{ProviderSpec, PROVIDERS};
use wingman_tui::modal::{LoginPayload, LoginTask};

use super::admin::{self, EditError};
use super::http::{self, Request};

/// Claude Code is a provider the TUI wizard does not list: it runs on the
/// user's Claude subscription through the `claude` CLI, so there is no key to
/// enter. Offered here so it can be chosen as the default from the panel.
const CLAUDE_CODE: ProviderSpec = ProviderSpec {
    id: wingman_core::claude_code::PROVIDER_ID,
    label: "Claude Code (subscription)",
    default_model: "claude-opus-4-7",
    needs_key: false,
    needs_oauth: false,
    default_base_url: None,
};

fn all() -> impl Iterator<Item = &'static ProviderSpec> {
    std::iter::once(&CLAUDE_CODE).chain(PROVIDERS.iter())
}

fn spec(id: &str) -> Option<&'static ProviderSpec> {
    all().find(|p| p.id == id)
}

/// How a provider authenticates, as the panel needs to draw its form.
fn kind(p: &ProviderSpec) -> &'static str {
    if p.id == CLAUDE_CODE.id {
        "cli"
    } else if p.needs_oauth {
        "oauth"
    } else if p.needs_key {
        "api-key"
    } else {
        "local"
    }
}

/// Where a provider's credential comes from — never the credential itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    /// Stored in the OS keyring by `wingman login` or this API.
    Keychain,
    /// The config names a keyring entry that does not exist (deleted, or a
    /// different machine). The provider will fail until a key is saved again.
    KeychainMissing,
    /// `${VAR}` in the config, or the provider's conventional variable, and
    /// the variable is set.
    Env(String),
    /// A literal key written into `config.toml` — works, but in plain text.
    ConfigFile,
    /// Nothing found.
    None,
    /// Local servers and subscriptions need no key.
    NotNeeded,
}

/// Classify a provider's credential from its raw config value and the
/// environment. `keyring` answers whether an entry exists, and `env` stands in
/// for `std::env::var`, so this can be tested without either.
pub fn classify(
    p: &ProviderSpec,
    raw_key: Option<&str>,
    keyring: &dyn Fn(&str) -> bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Source {
    let set = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
    let raw = raw_key.map(str::trim).filter(|s| !s.is_empty());

    if let Some(raw) = raw {
        if let Some(entry) = raw.strip_prefix("keyring:") {
            return if keyring(entry) {
                Source::Keychain
            } else {
                Source::KeychainMissing
            };
        }
        if let Some(var) = raw.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
            if set(var) {
                return Source::Env(var.to_string());
            }
        } else {
            return Source::ConfigFile;
        }
    }
    if p.needs_oauth && keyring(p.id) {
        return Source::Keychain;
    }
    if let Some(var) = crate::runtime::api_key_env_var(p.id) {
        if set(var) {
            return Source::Env(var.to_string());
        }
    }
    if !p.needs_key && !p.needs_oauth {
        Source::NotNeeded
    } else {
        Source::None
    }
}

fn source_json(s: &Source) -> Value {
    match s {
        Source::Keychain => json!({ "kind": "keychain" }),
        Source::KeychainMissing => json!({ "kind": "keychain-missing" }),
        Source::Env(var) => json!({ "kind": "env", "var": var }),
        Source::ConfigFile => json!({ "kind": "config" }),
        Source::None => json!({ "kind": "none" }),
        Source::NotNeeded => json!({ "kind": "not-needed" }),
    }
}

/// The global config file as a raw table, or empty when there is none yet.
/// Raw rather than [`wingman_config::Config`] because the loaded config has
/// already resolved `${VAR}` placeholders into the keys themselves, and the
/// question here is where a key comes from.
fn raw_global() -> toml::Table {
    wingman_config::global_config_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.parse::<toml::Table>().ok())
        .unwrap_or_default()
}

fn str_at<'a>(t: &'a toml::Table, key: &str) -> Option<&'a str> {
    t.get(key).and_then(|v| v.as_str())
}

/// `GET /v1/providers`
pub async fn list(sock: &mut TcpStream) -> std::io::Result<()> {
    let doc = tokio::task::spawn_blocking(|| {
        let raw = raw_global();
        let empty = toml::Table::new();
        let configured = raw
            .get("providers")
            .and_then(|v| v.as_table())
            .unwrap_or(&empty);
        let default_provider = str_at(&raw, "default_provider").map(str::to_string);
        // Keyring lookups only for providers whose config points at one, or
        // OAuth providers whose token lives there — not 75 blind probes.
        let keyring = |id: &str| secrets::load(id).ok().flatten().is_some();
        let env = |name: &str| std::env::var(name).ok();

        let providers: Vec<Value> = all()
            .map(|p| {
                let section = configured.get(p.id).and_then(|v| v.as_table());
                let source = classify(
                    p,
                    section.and_then(|s| str_at(s, "api_key")),
                    &keyring,
                    &env,
                );
                let ready = !matches!(source, Source::None | Source::KeychainMissing);
                let is_default = default_provider.as_deref() == Some(p.id);
                json!({
                    "id": p.id,
                    "label": p.label,
                    "kind": kind(p),
                    "env_var": crate::runtime::api_key_env_var(p.id),
                    "default_model": p.default_model,
                    "default_base_url": p.default_base_url,
                    // Connected: the user set it up (a config section), or it
                    // is the default. An env var alone does not count — it
                    // makes the provider *usable*, which `ready` reports.
                    "connected": section.is_some() || is_default,
                    "ready": ready,
                    "credential": source_json(&source),
                    "model": section.and_then(|s| str_at(s, "model")),
                    "base_url": section.and_then(|s| str_at(s, "base_url")),
                    "is_default": is_default,
                })
            })
            .collect();

        json!({
            "default_provider": default_provider,
            "default_model": str_at(&raw, "default_model"),
            "providers": providers,
        })
    })
    .await;

    match doc {
        Ok(doc) => http::write_json(sock, 200, &doc).await,
        Err(e) => http::write_err(sock, 500, &format!("reading providers: {e}")).await,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConnectBody {
    /// A new key. Absent means "keep whatever is stored" — so the model or
    /// address can change without re-entering a key.
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Make this the provider Wingman uses when none is named.
    pub make_default: bool,
    /// Save without the live test (e.g. a server that is not running yet).
    pub skip_test: bool,
}

/// What a test or a connect will call with, after filling gaps from what is
/// already stored and from the provider's own defaults.
struct Resolved {
    spec: &'static ProviderSpec,
    new_key: Option<String>,
    key: Option<String>,
    model: String,
    base_url: Option<String>,
}

fn resolve(id: &str, body: ConnectBody) -> Result<Resolved, (u16, String)> {
    let spec = spec(id).ok_or((404, format!("unknown provider '{id}'")))?;
    if spec.needs_oauth {
        return Err((
            409,
            format!(
                "{} signs in through a browser on the machine running Wingman. \
                 Run `wingman login {id}` there.",
                spec.label
            ),
        ));
    }

    let raw = raw_global();
    let section = raw
        .get("providers")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get(id))
        .and_then(|v| v.as_table());

    let new_key = body
        .api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    // An existing key is resolved exactly as a run would resolve it.
    let stored = || {
        crate::runtime::check_config_value(section.and_then(|s| str_at(s, "api_key"))).or_else(
            || {
                crate::runtime::api_key_env_var(id)
                    .and_then(|v| std::env::var(v).ok())
                    .filter(|v| !v.trim().is_empty())
            },
        )
    };
    let key = new_key.clone().or_else(stored);
    if spec.needs_key && key.is_none() {
        return Err((400, format!("{} needs an API key.", spec.label)));
    }

    let model = body
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .or_else(|| section.and_then(|s| str_at(s, "model")).map(str::to_string))
        .unwrap_or_else(|| spec.default_model.to_string());
    let base_url = body
        .base_url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .or_else(|| {
            section
                .and_then(|s| str_at(s, "base_url"))
                .map(str::to_string)
        })
        .or_else(|| spec.default_base_url.map(str::to_string));

    Ok(Resolved {
        spec,
        new_key,
        key,
        model,
        base_url,
    })
}

async fn probe(r: &Resolved) -> Result<(), String> {
    // Claude Code has no endpoint to call: it shells out to `claude`, so the
    // check is that the binary is there.
    if r.spec.id == CLAUDE_CODE.id {
        return which("claude")
            .then_some(())
            .ok_or_else(|| "the `claude` command is not installed or not on PATH".to_string());
    }
    crate::login::run_login_task(LoginTask::Probe(LoginPayload {
        provider_id: r.spec.id.to_string(),
        api_key: r.key.clone(),
        base_url: r.base_url.clone(),
        model: r.model.clone(),
    }))
    .await
}

fn which(bin: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| exts.iter().any(|e| dir.join(format!("{bin}{e}")).is_file()))
    })
}

fn body(req: &Request) -> Result<ConnectBody, String> {
    req.json::<Option<ConnectBody>>()
        .map(Option::unwrap_or_default)
}

/// `POST /v1/providers/{id}/test` — call the provider with these settings and
/// save nothing.
pub async fn test(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let r = match body(req).map_err(|e| (400, e)).and_then(|b| resolve(id, b)) {
        Ok(r) => r,
        Err((code, msg)) => return http::write_err(sock, code, &msg).await,
    };
    match probe(&r).await {
        Ok(()) => http::write_json(sock, 200, &json!({ "ok": true, "model": r.model })).await,
        Err(e) => http::write_err(sock, 422, &format!("The test failed: {e}")).await,
    }
}

/// `PUT /v1/providers/{id}` — test, then store the key and record the provider.
pub async fn connect(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let b = match body(req) {
        Ok(b) => b,
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    let (make_default, skip_test) = (b.make_default, b.skip_test);
    let r = match resolve(id, b) {
        Ok(r) => r,
        Err((code, msg)) => return http::write_err(sock, code, &msg).await,
    };

    if !skip_test {
        if let Err(e) = probe(&r).await {
            return http::write_err(
                sock,
                422,
                &format!("The test failed, so nothing was saved: {e}"),
            )
            .await;
        }
    }

    // The keyring first, like the CLI; a plaintext key in the config only
    // when there is no keyring (a headless box), and the response says so.
    let mut api_key_value: Option<String> = None;
    let mut stored_in = "unchanged";
    if let Some(key) = r.new_key.clone() {
        let pid = r.spec.id.to_string();
        let key2 = key.clone();
        match tokio::task::spawn_blocking(move || secrets::store(&pid, &key2)).await {
            Ok(Ok(())) => {
                api_key_value = Some(format!("keyring:{}", r.spec.id));
                stored_in = "keychain";
            }
            _ => {
                api_key_value = Some(key);
                stored_in = "config";
            }
        }
    }

    let mut entry = serde_json::Map::new();
    entry.insert("model".into(), json!(r.model));
    if let Some(url) = &r.base_url {
        entry.insert("base_url".into(), json!(url));
    }
    if let Some(v) = api_key_value {
        entry.insert("api_key".into(), json!(v));
    }
    let mut patch = json!({ "providers": { r.spec.id: Value::Object(entry) } });
    if make_default {
        patch["default_provider"] = json!(r.spec.id);
        patch["default_model"] = json!(format!("{}/{}", r.spec.id, r.model));
    }

    match admin::edit_global(|doc| admin::merge_json(doc, &patch)) {
        Ok(_) => {
            http::write_json(
                sock,
                200,
                &json!({
                    "connected": r.spec.id,
                    "model": r.model,
                    "stored_in": stored_in,
                    "is_default": make_default,
                    "tested": !skip_test,
                }),
            )
            .await
        }
        Err(e) => write_edit_err(sock, e).await,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DefaultBody {
    model: Option<String>,
}

/// `POST /v1/providers/{id}/default` — use this provider when none is named.
pub async fn make_default(id: &str, req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some(spec) = spec(id) else {
        return http::write_err(sock, 404, &format!("unknown provider '{id}'")).await;
    };
    let b: DefaultBody = req
        .json::<Option<DefaultBody>>()
        .ok()
        .flatten()
        .unwrap_or_default();
    let raw = raw_global();
    let model = b
        .model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            raw.get("providers")
                .and_then(|v| v.get(id))
                .and_then(|s| s.get("model"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| spec.default_model.to_string());

    let patch = json!({
        "default_provider": id,
        "default_model": format!("{id}/{model}"),
        "providers": { id: { "model": model } },
    });
    match admin::edit_global(|doc| admin::merge_json(doc, &patch)) {
        Ok(_) => http::write_json(sock, 200, &json!({ "default": id, "model": model })).await,
        Err(e) => write_edit_err(sock, e).await,
    }
}

/// `DELETE /v1/providers/{id}` — forget the key and the provider's settings.
pub async fn disconnect(id: &str, sock: &mut TcpStream) -> std::io::Result<()> {
    let Some(spec) = spec(id) else {
        return http::write_err(sock, 404, &format!("unknown provider '{id}'")).await;
    };
    if str_at(&raw_global(), "default_provider") == Some(id) {
        return http::write_err(
            sock,
            409,
            &format!(
                "{} is your default provider. Choose another default first.",
                spec.label
            ),
        )
        .await;
    }

    let pid = id.to_string();
    let keyring = tokio::task::spawn_blocking(move || {
        let r = secrets::delete(&pid);
        // ChatGPT keeps its refresh token in a second entry.
        if pid == "chatgpt" {
            let _ = secrets::delete("chatgpt_refresh");
        }
        r
    })
    .await;
    let keyring_note = match keyring {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{e}")),
        Err(e) => Some(format!("{e}")),
    };

    let section = id.to_string();
    match admin::edit_global(move |doc| {
        remove_provider(doc, &section);
        Ok(())
    }) {
        Ok(_) => {
            http::write_json(
                sock,
                200,
                &json!({ "disconnected": id, "keyring_error": keyring_note }),
            )
            .await
        }
        Err(e) => write_edit_err(sock, e).await,
    }
}

/// Drop `[providers.<id>]`, leaving every other line of the file as it was.
pub fn remove_provider(doc: &mut toml_edit::DocumentMut, id: &str) {
    if let Some(t) = doc.get_mut("providers").and_then(|p| p.as_table_like_mut()) {
        t.remove(id);
    }
}

async fn write_edit_err(sock: &mut TcpStream, e: EditError) -> std::io::Result<()> {
    http::write_err(sock, e.status, &e.message).await
}

pub fn schema() -> Vec<Value> {
    vec![
        json!({ "method": "GET", "path": "/v1/providers", "auth": true,
                "about": "every provider with connection state and where its key comes from — never the key" }),
        json!({ "method": "POST", "path": "/v1/providers/{id}/test", "auth": true,
                "body": { "api_key": "string?", "model": "string?", "base_url": "string?" },
                "about": "call the provider with these settings; saves nothing" }),
        json!({ "method": "PUT", "path": "/v1/providers/{id}", "auth": true,
                "body": { "api_key": "string?", "model": "string?", "base_url": "string?",
                          "make_default": "bool", "skip_test": "bool" },
                "about": "test, store the key in the OS keyring, record the provider in the global config" }),
        json!({ "method": "POST", "path": "/v1/providers/{id}/default", "auth": true,
                "body": { "model": "string?" },
                "about": "make this the default provider" }),
        json!({ "method": "DELETE", "path": "/v1/providers/{id}", "auth": true,
                "about": "remove the keyring entry and [providers.{id}]; refuses the default provider" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &'static str, needs_key: bool) -> ProviderSpec {
        ProviderSpec {
            id,
            label: id,
            default_model: "m",
            needs_key,
            needs_oauth: false,
            default_base_url: None,
        }
    }

    const NO_ENV: &dyn Fn(&str) -> Option<String> = &|_| None;

    #[test]
    fn a_keyring_marker_is_the_keychain_if_the_entry_exists() {
        let anthropic = p("anthropic", true);
        assert_eq!(
            classify(&anthropic, Some("keyring:anthropic"), &|_| true, NO_ENV),
            Source::Keychain
        );
        assert_eq!(
            classify(&anthropic, Some("keyring:anthropic"), &|_| false, NO_ENV),
            Source::KeychainMissing
        );
    }

    #[test]
    fn a_placeholder_counts_only_when_its_variable_is_set() {
        let anthropic = p("anthropic", true);
        let env = |n: &str| (n == "MY_KEY").then(|| "sk".to_string());
        assert_eq!(
            classify(&anthropic, Some("${MY_KEY}"), &|_| false, &env),
            Source::Env("MY_KEY".into())
        );
        assert_eq!(
            classify(&anthropic, Some("${OTHER}"), &|_| false, &env),
            Source::None
        );
    }

    #[test]
    fn a_literal_key_is_reported_as_plain_text_in_the_file() {
        assert_eq!(
            classify(&p("openai", true), Some("sk-abc"), &|_| false, NO_ENV),
            Source::ConfigFile
        );
    }

    #[test]
    fn the_conventional_variable_is_found_without_any_config() {
        let env = |n: &str| (n == "ANTHROPIC_API_KEY").then(|| "sk".to_string());
        assert_eq!(
            classify(&p("anthropic", true), None, &|_| false, &env),
            Source::Env("ANTHROPIC_API_KEY".into())
        );
    }

    #[test]
    fn local_servers_need_nothing() {
        assert_eq!(
            classify(&p("ollama", false), None, &|_| false, NO_ENV),
            Source::NotNeeded
        );
    }

    #[test]
    fn disconnecting_removes_only_that_provider() {
        let mut doc: toml_edit::DocumentMut = "# mine\ndefault_provider = \"openai\"\n\n\
            [providers.anthropic]\napi_key = \"keyring:anthropic\"\n\n\
            # keep this comment\n[providers.openai]\nmodel = \"gpt-4.1\"\n"
            .parse()
            .unwrap();
        remove_provider(&mut doc, "anthropic");
        let out = doc.to_string();
        assert!(!out.contains("anthropic"), "{out}");
        assert!(out.contains("# keep this comment"), "{out}");
        assert!(out.contains("model = \"gpt-4.1\""), "{out}");
        assert!(out.contains("# mine"), "{out}");
    }

    #[test]
    fn claude_code_is_offered_alongside_the_wizard_table() {
        assert!(spec("claude-code").is_some());
        assert!(spec("anthropic").is_some());
        assert_eq!(kind(spec("claude-code").unwrap()), "cli");
        assert_eq!(kind(spec("chatgpt").unwrap()), "oauth");
    }
}
