//! The two routes that are not just "run a subcommand and return stdout":
//! the `/exec` escape hatch, and patching global config.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;

use super::http::{self, Request};
use super::projects::Project;
use super::{argv, child, table, ServeState};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ExecBody {
    /// Argument vector — never a command line. There is no shell in this
    /// path, so nothing here is parsed for `;`, `|`, or quotes.
    pub args: Vec<String>,
    /// Stream stdout/stderr as SSE instead of buffering to a JSON blob.
    pub stream: bool,
}

/// `POST /v1/projects/{p}/exec` — run any allowed subcommand.
///
/// This is the "everything else" route: whatever the CLI grows tomorrow is
/// reachable through it the day it lands, without a new endpoint. What keeps
/// it from being a remote shell is [`argv::sanitize`] — argv only, known
/// subcommands only, refusal list, and the permission ceiling — plus the fact
/// that at the default ceiling the child cannot run unrestricted shell
/// commands either. At a `yolo` ceiling it *is* remote code execution by
/// design, which is why that ceiling needs `--allow-yolo` at launch.
pub async fn exec(
    state: &Arc<ServeState>,
    project: &Project,
    req: &Request,
    sock: &mut TcpStream,
) -> std::io::Result<()> {
    let body: ExecBody = match req.json::<Option<ExecBody>>() {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    let args = match argv::sanitize(&body.args, state.ceiling) {
        Ok(a) => a,
        Err(argv::Rejected::BadRequest(m)) => return http::write_err(sock, 400, &m).await,
        Err(argv::Rejected::Forbidden(m)) => return http::write_err(sock, 403, &m).await,
    };

    if !body.stream {
        return table::execute(state, project, &args, sock).await;
    }

    let mut cmd = match child::command(&project.root, state.ceiling) {
        Ok(c) => c,
        Err(e) => return http::write_err(sock, 500, &format!("resolving executable: {e}")).await,
    };
    cmd.args(&args);
    let timeout = Duration::from_secs(state.cfg.serve.request_timeout_secs.max(1));
    child::stream_events(cmd, sock, timeout).await
}

/// `GET /v1/config` — the merged effective config, secrets removed.
pub async fn get_config(state: &Arc<ServeState>, sock: &mut TcpStream) -> std::io::Result<()> {
    let mut value = match serde_json::to_value(&state.cfg) {
        Ok(v) => v,
        Err(e) => return http::write_err(sock, 500, &format!("serialising config: {e}")).await,
    };
    redact(&mut value);
    http::write_json(sock, 200, &value).await
}

/// Blank out anything that is a credential.
///
/// `Config` holds resolved secrets in memory — provider API keys, the Slack
/// signing secret, notification webhook URLs, and this server's own token.
/// Returning them would turn a read-only config peek into credential
/// exfiltration for anyone holding the API token, which is a strictly larger
/// authority than the API is supposed to grant.
fn redact(value: &mut Value) {
    const SECRET_KEYS: &[&str] = &[
        "api_key",
        "token",
        "webhook_secret",
        "slack_signing_secret",
        "webhooks",
        "url",
        "endpoint",
    ];
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if SECRET_KEYS.contains(&k.as_str()) && !v.is_null() {
                    *v = Value::String("<redacted>".into());
                } else {
                    redact(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// `PATCH /v1/config` — merge a TOML-shaped JSON object into the **global**
/// config file.
///
/// Two things it will not do. It never writes a project's
/// `.wingman/config.toml`: those are the untrusted layer, and an API that
/// could write them would be a way to smuggle executable keys into a repo.
/// And it never writes `[serve]` — a server that can rewrite its own token,
/// ceiling, or project allowlist has no ceiling at all.
pub async fn patch_config(req: &Request, sock: &mut TcpStream) -> std::io::Result<()> {
    let patch: Value = match req.json() {
        Ok(v) => v,
        Err(e) => return http::write_err(sock, 400, &e).await,
    };
    let Value::Object(patch) = patch else {
        return http::write_err(sock, 400, "body must be a JSON object of config keys").await;
    };
    if patch.contains_key("serve") {
        return http::write_err(
            sock,
            403,
            "[serve] cannot be changed through the API it configures — edit the config file",
        )
        .await;
    }

    let path = match wingman_config::global_config_path() {
        Ok(p) => p,
        Err(e) => return http::write_err(sock, 500, &format!("resolving config path: {e}")).await,
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Table = match toml::from_str(&existing) {
        Ok(t) => t,
        Err(e) => {
            return http::write_err(
                sock,
                500,
                &format!("existing config is not valid TOML: {e}"),
            )
            .await
        }
    };

    for (k, v) in patch {
        match json_to_toml(&v) {
            Some(value) => merge(&mut doc, &k, value),
            None => {
                return http::write_err(
                    sock,
                    400,
                    &format!("value for '{k}' is not expressible in TOML"),
                )
                .await
            }
        }
    }

    // Round-trip through the real parser before writing: a patch that
    // produces a config Wingman cannot load would take the whole CLI down,
    // not just this request.
    let rendered = match toml::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(e) => return http::write_err(sock, 500, &format!("rendering config: {e}")).await,
    };
    if let Err(e) = toml::from_str::<wingman_config::Config>(&rendered) {
        return http::write_err(sock, 400, &format!("patch produces an invalid config: {e}")).await;
    }

    if let Err(e) = write_atomic(&path, &rendered) {
        return http::write_err(sock, 500, &format!("writing config: {e}")).await;
    }
    http::write_json(
        sock,
        200,
        &json!({ "written": path.to_string_lossy(), "restart_required": true }),
    )
    .await
}

/// Merge one top-level key, recursing into tables so a patch of
/// `{"tokens":{"max_usd_per_session":5}}` does not drop the rest of
/// `[tokens]`.
fn merge(doc: &mut toml::Table, key: &str, value: toml::Value) {
    match (doc.get_mut(key), value) {
        (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
            for (k, v) in incoming {
                merge(existing, &k, v);
            }
        }
        (_, value) => {
            doc.insert(key.to_string(), value);
        }
    }
}

fn json_to_toml(v: &Value) -> Option<toml::Value> {
    Some(match v {
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml::Value::Integer(i),
            (_, Some(f)) => toml::Value::Float(f),
            _ => return None,
        },
        Value::Array(items) => {
            toml::Value::Array(items.iter().map(json_to_toml).collect::<Option<Vec<_>>>()?)
        }
        Value::Object(map) => {
            let mut table = toml::Table::new();
            for (k, v) in map {
                table.insert(k.clone(), json_to_toml(v)?);
            }
            toml::Value::Table(table)
        }
        // TOML has no null; a key set to null is a request to remove it,
        // which `merge` cannot express — reject rather than guess.
        Value::Null => return None,
    })
}

/// Write through a temp file and rename, so a crash mid-write leaves the old
/// config intact rather than a truncated one that fails to parse.
fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

pub fn schema() -> Vec<Value> {
    vec![
        json!({ "method": "POST", "path": "/v1/projects/{project}/exec", "auth": true,
                "body": { "args": "string[] — argv, not a command line", "stream": "bool" },
                "about": "run any allowed subcommand; refuses serve/login/logout and clamps --mode to the ceiling" }),
        json!({ "method": "GET", "path": "/v1/config", "auth": true,
                "about": "merged effective config, credentials redacted" }),
        json!({ "method": "PATCH", "path": "/v1/config", "auth": true,
                "body": "TOML-shaped JSON object merged into the global config",
                "about": "writes the global config file only; refuses [serve]" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_leave_in_a_config_read() {
        let mut v = json!({
            "providers": { "anthropic": { "api_key": "sk-real-secret", "model": "opus" } },
            "serve": { "token": "the-api-token", "addr": "0.0.0.0:8787" },
            "pilot": { "daemon": { "slack_signing_secret": "shh" } },
        });
        redact(&mut v);
        let text = v.to_string();
        assert!(!text.contains("sk-real-secret"), "{text}");
        assert!(!text.contains("the-api-token"), "{text}");
        assert!(!text.contains("shh"), "{text}");
        // Non-secret neighbours survive, or the endpoint is useless.
        assert_eq!(v["providers"]["anthropic"]["model"], "opus");
        assert_eq!(v["serve"]["addr"], "0.0.0.0:8787");
    }

    #[test]
    fn patching_a_table_keeps_its_other_keys() {
        let mut doc: toml::Table =
            toml::from_str("[tokens]\nmax_usd_per_session = 1.0\ntool_output_max_lines = 200\n")
                .unwrap();
        let patch = json_to_toml(&json!({ "max_usd_per_session": 5 })).unwrap();
        merge(&mut doc, "tokens", patch);
        let tokens = doc["tokens"].as_table().unwrap();
        assert_eq!(tokens["max_usd_per_session"].as_integer(), Some(5));
        assert_eq!(tokens["tool_output_max_lines"].as_integer(), Some(200));
    }

    #[test]
    fn json_nulls_are_rejected_rather_than_guessed_at() {
        assert!(json_to_toml(&json!(null)).is_none());
        assert!(json_to_toml(&json!({ "a": null })).is_none());
    }

    #[test]
    fn scalars_arrays_and_nested_tables_convert() {
        let v = json_to_toml(&json!({
            "s": "x", "b": true, "i": 3, "f": 1.5,
            "arr": ["a", "b"],
            "nested": { "k": "v" },
        }))
        .unwrap();
        let t = v.as_table().unwrap();
        assert_eq!(t["s"].as_str(), Some("x"));
        assert_eq!(t["b"].as_bool(), Some(true));
        assert_eq!(t["i"].as_integer(), Some(3));
        assert_eq!(t["f"].as_float(), Some(1.5));
        assert_eq!(t["arr"].as_array().unwrap().len(), 2);
        assert_eq!(t["nested"].as_table().unwrap()["k"].as_str(), Some("v"));
    }
}
