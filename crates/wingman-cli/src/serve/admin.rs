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

/// Keys whose values are credentials, blanked on read.
///
/// Emitted by `GET /v1/config/schema` as well as applied by [`redact`], so a
/// settings UI can render these as redacted rather than as empty inputs that
/// would `PATCH` a credential away. One list, two consumers — a copy in the
/// panel would rot the first time a secret key was added here.
pub const SECRET_KEYS: &[&str] = &[
    "api_key",
    "token",
    "webhook_secret",
    "slack_signing_secret",
    "webhooks",
    "url",
    "endpoint",
];

/// Top-level sections the API refuses to write.
///
/// `patch_config` returns `403` for these. The UI is told rather than left to
/// discover it by having a save rejected.
pub const READONLY_SECTIONS: &[&str] = &["serve"];

/// `GET /v1/config/schema` — what the settings UI builds its forms from.
///
/// The schema is derived from the `wingman-config` structs themselves, so a
/// new field appears in the panel with its documentation without anyone
/// hand-writing a form for it. `///` comments become `description`, which is
/// the difference between a usable settings screen and a wall of unlabelled
/// inputs.
///
/// Defaults ride along separately: `schemars` records a default only where one
/// is declared in a way it can see, whereas serialising `Config::default()`
/// yields the value every field actually falls back to.
pub async fn get_config_schema(sock: &mut TcpStream) -> std::io::Result<()> {
    let schema = wingman_config::json_schema();
    let mut defaults = match serde_json::to_value(wingman_config::Config::default()) {
        Ok(v) => v,
        Err(e) => return http::write_err(sock, 500, &format!("serialising defaults: {e}")).await,
    };
    // The defaults are a config value like any other, so the same credential
    // rule applies — a default token is still a token.
    redact(&mut defaults);

    let path = wingman_config::global_config_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    http::write_json(
        sock,
        200,
        &json!({
            "schema": schema,
            "defaults": defaults,
            "redacted_keys": SECRET_KEYS,
            "readonly_sections": READONLY_SECTIONS,
            // Writes land in the global file only, never a repo's
            // `.wingman/config.toml`. A UI that did not say so would be
            // silently not writing where the user thinks it is.
            "writes_to": path,
        }),
    )
    .await
}

/// Blank out anything that is a credential.
///
/// `Config` holds resolved secrets in memory — provider API keys, the Slack
/// signing secret, notification webhook URLs, and this server's own token.
/// Returning them would turn a read-only config peek into credential
/// exfiltration for anyone holding the API token, which is a strictly larger
/// authority than the API is supposed to grant.
fn redact(value: &mut Value) {
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

    // Edited as a document, not re-serialised from a `toml::Table`. Parsing to
    // a table and printing it back drops every comment in the user's config
    // and reorders the whole file, so a one-field change through the panel
    // would arrive as a total rewrite — and their annotations would be gone
    // with no way to notice until they went looking.
    let mut doc: toml_edit::DocumentMut = match existing.parse() {
        Ok(d) => d,
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
        match json_to_item(&v) {
            Some(item) => merge_item(doc.as_table_mut(), &k, item),
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
    let rendered = doc.to_string();
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

/// Merge one key into a table, recursing so a patch of
/// `{"tokens":{"max_usd_per_session":5}}` does not drop the rest of
/// `[tokens]`.
///
/// Only the keys the patch names are touched. Every other key keeps its
/// position, its formatting and the comments attached to it, which is the
/// whole reason this operates on a `toml_edit` document rather than a
/// re-serialised table.
fn merge_item(table: &mut toml_edit::Table, key: &str, incoming: toml_edit::Item) {
    match (table.get_mut(key), incoming) {
        // Both sides are tables: recurse, so untouched sub-keys survive.
        (Some(existing), incoming) if existing.is_table_like() && incoming.is_table_like() => {
            let Some(incoming) = incoming.as_table_like() else {
                return;
            };
            // Collected first because the borrow of `existing` has to end
            // before the recursive call can take it mutably.
            let entries: Vec<(String, toml_edit::Item)> = incoming
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect();
            let Some(existing) = existing.as_table_mut() else {
                return;
            };
            for (k, v) in entries {
                merge_item(existing, &k, v);
            }
        }
        (Some(existing), incoming) => {
            // Replace the value but keep the key's own decor — the comment
            // someone wrote above a setting still describes that setting after
            // its value changes.
            let decor = existing.as_value().map(|v| v.decor().clone());
            *existing = incoming;
            if let (Some(decor), Some(value)) = (decor, existing.as_value_mut()) {
                *value.decor_mut() = decor;
            }
        }
        (None, incoming) => {
            table.insert(key, incoming);
        }
    }
}

fn json_to_item(v: &Value) -> Option<toml_edit::Item> {
    use toml_edit::Item;
    Some(match v {
        Value::Object(map) => {
            let mut table = toml_edit::Table::new();
            // Sub-tables written by a patch are implicit, so a patch naming
            // only `a.b.c` does not emit an empty `[a.b]` header the user
            // never asked for.
            table.set_implicit(true);
            for (k, v) in map {
                table.insert(k, json_to_item(v)?);
            }
            Item::Table(table)
        }
        other => Item::Value(json_to_value(other)?),
    })
}

fn json_to_value(v: &Value) -> Option<toml_edit::Value> {
    use toml_edit::Value as EValue;
    Some(match v {
        Value::String(s) => EValue::from(s.as_str()),
        Value::Bool(b) => EValue::from(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => EValue::from(i),
            (_, Some(f)) => EValue::from(f),
            _ => return None,
        },
        Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for it in items {
                arr.push(json_to_value(it)?);
            }
            EValue::Array(arr)
        }
        Value::Object(map) => {
            // An object nested inside an array cannot become a `[table]`
            // header, so it is written as an inline table.
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in map {
                t.insert(k, json_to_value(v)?);
            }
            EValue::InlineTable(t)
        }
        // TOML has no null; a key set to null is a request to remove it,
        // which this cannot express — reject rather than guess.
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

    /// Apply a patch the way `patch_config` does, minus the file I/O.
    fn patched(existing: &str, patch: Value) -> String {
        let mut doc: toml_edit::DocumentMut = existing.parse().unwrap();
        let Value::Object(map) = patch else {
            panic!("patch must be an object")
        };
        for (k, v) in map {
            merge_item(doc.as_table_mut(), &k, json_to_item(&v).unwrap());
        }
        doc.to_string()
    }

    #[test]
    fn patching_a_table_keeps_its_other_keys() {
        let out = patched(
            "[tokens]
max_usd_per_session = 1.0
tool_output_max_lines = 200
",
            json!({ "tokens": { "max_usd_per_session": 5 } }),
        );
        assert!(out.contains("max_usd_per_session = 5"), "{out}");
        assert!(out.contains("tool_output_max_lines = 200"), "{out}");
    }

    /// The regression this rewrite exists for.
    ///
    /// The previous implementation parsed to a `toml::Table` and printed it
    /// back, which discarded every comment and reordered the whole file. A
    /// one-field save from the web panel arrived as a total rewrite, and the
    /// user's annotations were gone with nothing to notice it by.
    #[test]
    fn a_patch_preserves_comments_ordering_and_formatting() {
        let existing = "# Wingman configuration — hand-tuned, do not reformat.
default_provider = \"openrouter\"

[tokens]
# Compact aggressively; this box has little RAM.
compact_at_tokens = 120000
tool_output_max_lines = 400

[verify]
turn_gate = \"auto\"
";
        let out = patched(
            existing,
            json!({ "tokens": { "compact_at_tokens": 64000 } }),
        );

        assert!(
            out.contains("# Wingman configuration"),
            "banner comment lost:
{out}"
        );
        assert!(
            out.contains("# Compact aggressively"),
            "the comment above the edited key was lost:
{out}"
        );
        assert!(out.contains("compact_at_tokens = 64000"), "{out}");
        // Untouched neighbours keep their values and their order.
        assert!(out.contains("tool_output_max_lines = 400"), "{out}");
        assert!(
            out.find("[tokens]").unwrap() < out.find("[verify]").unwrap(),
            "sections were reordered:
{out}"
        );
        // And nothing else in the file moved at all.
        assert_eq!(out, existing.replace("120000", "64000"));
    }

    #[test]
    fn a_new_key_is_added_without_an_empty_parent_header() {
        let out = patched("", json!({ "verify": { "browser": { "tolerance": 5 } } }));
        assert!(out.contains("tolerance = 5"), "{out}");
        // `[verify]` is implicit: the patch never named a value directly under
        // it, so emitting the header would add a section nobody asked for.
        assert!(
            !out.contains(
                "[verify]
"
            ),
            "{out}"
        );
    }

    #[test]
    fn json_nulls_are_rejected_rather_than_guessed_at() {
        // TOML has no null. Treating it as "remove this key" would be a guess
        // at intent that the API has no way to confirm.
        assert!(json_to_item(&json!(null)).is_none());
        assert!(json_to_item(&json!({ "a": null })).is_none());
    }

    #[test]
    fn scalars_arrays_and_nested_tables_convert() {
        let out = patched(
            "",
            json!({ "s": "x", "b": true, "i": 3, "f": 1.5, "arr": ["a", "b"],
                    "nested": { "k": "v" } }),
        );
        assert!(out.contains("s = \"x\""), "{out}");
        assert!(out.contains("b = true"), "{out}");
        assert!(out.contains("i = 3"), "{out}");
        assert!(out.contains("f = 1.5"), "{out}");
        assert!(out.contains(r#"arr = ["a", "b"]"#), "{out}");
        assert!(out.contains("k = \"v\""), "{out}");
    }
}
