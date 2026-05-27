//! Configuration loading and merging for arccode.
//!
//! Resolution order (lowest to highest precedence):
//!   1. Built-in defaults
//!   2. Global config at `~/.arccode/config.toml`
//!   3. Project config at `<project>/.arccode/config.toml`
//!   4. Environment variables (`ARCCODE_*`)
//!   5. CLI flag overrides (applied by the caller via [`Config::apply_overrides`])
//!
//! Per the plan: global `~/.arccode/` holds config/creds/model cache; per-project
//! `.arccode/` holds session log overrides and the repo index.

mod paths;
pub mod secrets;

pub use paths::{
    ensure_global_dir, ensure_global_logs_dir, find_project_root, global_config_path,
    global_credentials_path, global_dir, global_logs_dir, project_dir, ProjectPaths,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("toml parse error in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("toml serialize error: {0}")]
    Serialize(Box<toml::ser::Error>),

    #[error("could not determine home directory")]
    NoHome,

    #[error("invalid env var {name}={value}: {reason}")]
    BadEnv {
        name: String,
        value: String,
        reason: String,
    },
}

impl From<toml::ser::Error> for ConfigError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Serialize(Box::new(e))
    }
}

/// Permission model — controls when the user is prompted before writes / shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Reads/searches free; every write or shell call prompts.
    #[default]
    ReadOnly,
    /// Writes/shell inside the project tree auto-allowed; out-of-tree paths
    /// and a denylist of destructive shell patterns still prompt.
    AutoEdit,
    /// No prompts. Only enabled per-session via `--yolo`; never persisted.
    Yolo,
}

impl std::str::FromStr for PermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "ro" => Ok(Self::ReadOnly),
            "auto-edit" | "autoedit" | "auto" => Ok(Self::AutoEdit),
            "yolo" => Ok(Self::Yolo),
            other => Err(format!(
                "unknown permission mode '{other}' (expected read-only, auto-edit, yolo)"
            )),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReadOnly => "read-only",
            Self::AutoEdit => "auto-edit",
            Self::Yolo => "yolo",
        })
    }
}

/// Per-project tool settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Additional shell patterns to always deny even in yolo mode.
    /// e.g. ["rm -rf /", "sudo"]
    #[serde(default)]
    pub shell_denylist: Vec<String>,
    /// Override the tool output budget (max lines per tool call). 0 = use global default.
    #[serde(default)]
    pub tool_output_max_lines: Option<u32>,
    /// Comma-separated list of tools to disable for this project.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

/// Top-level merged configuration. Constructed via [`Config::load`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub permission_mode: PermissionMode,

    /// Per-provider configuration, keyed by stable provider id
    /// (e.g. "anthropic", "openai", "gemini", "ollama", "openrouter").
    pub providers: BTreeMap<String, ProviderConfig>,

    pub tui: TuiConfig,
    pub tokens: TokenConfig,
    pub router: RouterConfig,
    pub logging: LoggingConfig,

    /// MCP servers, keyed by user-chosen short name. Activated in M3.
    pub mcp: BTreeMap<String, McpServerConfig>,

    /// Per-project tool settings.
    #[serde(default)]
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// API key. Resolved against env at load time if it looks like `${ENV_VAR}`.
    pub api_key: Option<String>,
    /// Override base URL — used for OpenAI-compatible aggregators
    /// (OpenRouter, LiteLLM, LM Studio, vLLM, etc.).
    pub base_url: Option<String>,
    /// Optional explicit model id for this provider.
    pub model: Option<String>,
    /// Free-form extras passed through to provider impls.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    pub theme: String,
    pub show_token_usage: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            show_token_usage: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokenConfig {
    /// Compact when used context exceeds this many tokens.
    pub compact_at_tokens: u32,
    /// Cap on a single tool output before head/tail truncation kicks in.
    pub tool_output_max_lines: u32,
    /// Enable provider prompt caching where supported.
    pub prompt_cache: bool,
}

impl Default for TokenConfig {
    fn default() -> Self {
        Self {
            compact_at_tokens: 120_000,
            tool_output_max_lines: 400,
            prompt_cache: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// "Fast" model used for classification, summarization, and recap.
    /// Form: `provider/model_id`, e.g. `anthropic/claude-haiku-4-5-20251001`.
    pub fast_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing-subscriber` env-filter directive.
    pub filter: String,
    /// Write logs to a file under `~/.arccode/logs/`.
    pub file: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "info,arccode=info".into(),
            file: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Transport: "stdio" (default) or "http".
    pub transport: String,
    /// Command to spawn for stdio transport.
    pub command: Option<String>,
    pub args: Vec<String>,
    /// URL for http transport.
    pub url: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".into(),
            command: None,
            args: Vec::new(),
            url: None,
        }
    }
}

impl Config {
    /// Load configuration with the documented merge order. Either path may
    /// be `None` to skip that layer (used by tests and `config init`).
    ///
    /// Files are merged at the raw-TOML level so that absent sections in the
    /// project file do not clobber the global file's values.
    pub fn load(
        global_path: Option<&Path>,
        project_path: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let mut merged = toml::Table::new();

        if let Some(p) = global_path {
            if p.exists() {
                merge_table(&mut merged, read_raw(p)?);
            }
        }
        if let Some(p) = project_path {
            if p.exists() {
                merge_table(&mut merged, read_raw(p)?);
            }
        }

        let mut cfg: Config =
            toml::Value::Table(merged)
                .try_into()
                .map_err(|source| ConfigError::Parse {
                    path: PathBuf::from("<merged>"),
                    source: Box::new(source),
                })?;

        cfg.apply_env(std::env::vars())?;
        cfg.resolve_env_placeholders();
        Ok(cfg)
    }

    /// Apply `ARCCODE_*` environment variables.
    ///
    /// Currently supported:
    ///   - `ARCCODE_MODEL`            -> `default_model`
    ///   - `ARCCODE_PROVIDER`         -> `default_provider`
    ///   - `ARCCODE_PERMISSION_MODE`  -> `permission_mode`
    ///   - `ARCCODE_LOG`              -> `logging.filter`
    ///   - `ARCCODE_<PROVIDER>_API_KEY`  -> providers[<provider>].api_key
    ///   - `ARCCODE_<PROVIDER>_BASE_URL` -> providers[<provider>].base_url
    pub fn apply_env<I>(&mut self, vars: I) -> Result<(), ConfigError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        for (k, v) in vars {
            if !k.starts_with("ARCCODE_") {
                continue;
            }
            match k.as_str() {
                "ARCCODE_MODEL" => self.default_model = Some(v),
                "ARCCODE_PROVIDER" => self.default_provider = Some(v),
                "ARCCODE_PERMISSION_MODE" => {
                    self.permission_mode = v.parse().map_err(|e: String| ConfigError::BadEnv {
                        name: k.clone(),
                        value: v.clone(),
                        reason: e,
                    })?;
                }
                "ARCCODE_LOG" => self.logging.filter = v,
                _ => {
                    if let Some(rest) = k.strip_prefix("ARCCODE_") {
                        if let Some((provider, field)) = split_provider_field(rest) {
                            let entry = self
                                .providers
                                .entry(provider.to_ascii_lowercase())
                                .or_default();
                            match field {
                                "API_KEY" => entry.api_key = Some(v),
                                "BASE_URL" => entry.base_url = Some(v),
                                "MODEL" => entry.model = Some(v),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Replace `${ENV_VAR}` placeholders in string fields with the env value.
    fn resolve_env_placeholders(&mut self) {
        for p in self.providers.values_mut() {
            if let Some(s) = p.api_key.as_mut() {
                if let Some(name) = strip_env_placeholder(s) {
                    if let Ok(val) = std::env::var(name) {
                        *s = val;
                    }
                }
            }
        }
    }

    /// Render this config as TOML.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Atomically write this config to `path`. Writes to a sibling tmpfile
    /// then renames over the target so a crash mid-write can't leave a
    /// half-written config. Creates the parent directory if missing.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = self.to_toml_string()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|source| ConfigError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Persist a new default provider + model selection (and the per-provider
    /// model + optional base_url) to `path`. Reads the existing file if
    /// present so we don't overwrite unrelated sections; the provider's
    /// `api_key` is set to the marker `"keyring:<provider_id>"` so the
    /// runtime knows to look up the OS keyring.
    ///
    /// `base_url` is only persisted when `Some` — useful for local providers
    /// (LM Studio, Ollama, vLLM) whose default URL the user may have
    /// overridden in the login wizard.
    pub fn set_default_provider_and_save(
        path: &Path,
        provider_id: &str,
        model: &str,
        base_url: Option<&str>,
        with_keyring: bool,
    ) -> Result<(), ConfigError> {
        let mut cfg = if path.exists() {
            // Re-read the raw file (not the merged config) so we only edit
            // and write this one layer.
            let table = read_raw(path)?;
            toml::Value::Table(table)
                .try_into()
                .map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source: Box::new(source),
                })?
        } else {
            Config::default()
        };

        cfg.default_provider = Some(provider_id.to_string());
        cfg.default_model = Some(format!("{provider_id}/{model}"));

        let entry = cfg.providers.entry(provider_id.to_string()).or_default();
        entry.model = Some(model.to_string());
        if let Some(url) = base_url {
            entry.base_url = Some(url.to_string());
        }
        if with_keyring {
            entry.api_key = Some(format!("keyring:{provider_id}"));
        }

        cfg.save_atomic(path)
    }

    /// Starter config written by `arccode config init`.
    pub fn starter() -> Self {
        let providers: BTreeMap<String, ProviderConfig> = [
            (
                "anthropic".to_string(),
                ProviderConfig {
                    api_key: Some("${ANTHROPIC_API_KEY}".into()),
                    model: Some("claude-opus-4-7".into()),
                    ..Default::default()
                },
            ),
            (
                "openai".to_string(),
                ProviderConfig {
                    api_key: Some("${OPENAI_API_KEY}".into()),
                    model: Some("gpt-4.1".into()),
                    ..Default::default()
                },
            ),
            (
                "gemini".to_string(),
                ProviderConfig {
                    api_key: Some("${GOOGLE_API_KEY}".into()),
                    model: Some("gemini-2.5-pro".into()),
                    ..Default::default()
                },
            ),
            (
                "ollama".to_string(),
                ProviderConfig {
                    // Ollama exposes an OpenAI-compatible shim at /v1.
                    base_url: Some("http://localhost:11434/v1".into()),
                    model: Some("llama3.1:8b".into()),
                    ..Default::default()
                },
            ),
            (
                "openrouter".to_string(),
                ProviderConfig {
                    api_key: Some("${OPENROUTER_API_KEY}".into()),
                    model: Some("anthropic/claude-opus-4-7".into()),
                    ..Default::default()
                },
            ),
            (
                "lmstudio".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:1234/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "vllm".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:8000/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "litellm".to_string(),
                ProviderConfig {
                    api_key: Some("${LITELLM_API_KEY}".into()),
                    base_url: Some("http://localhost:4000/v1".into()),
                    model: Some("anthropic/claude-opus-4-7".into()),
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect();

        Config {
            default_provider: Some("anthropic".into()),
            permission_mode: PermissionMode::ReadOnly,
            providers,
            ..Default::default()
        }
    }
}

fn read_raw(path: &Path) -> Result<toml::Table, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Recursive table merge: keys in `overlay` overwrite `base`; sub-tables are
/// merged in turn so that a single key in the overlay does not clobber the
/// whole sub-table from the base.
fn merge_table(base: &mut toml::Table, overlay: toml::Table) {
    for (k, v_overlay) in overlay {
        match (base.remove(&k), v_overlay) {
            (Some(toml::Value::Table(mut bt)), toml::Value::Table(ot)) => {
                merge_table(&mut bt, ot);
                base.insert(k, toml::Value::Table(bt));
            }
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

fn split_provider_field(rest: &str) -> Option<(&str, &str)> {
    // ARCCODE_<PROVIDER>_<FIELD> where FIELD is one of API_KEY, BASE_URL, MODEL.
    // The provider name may contain underscores too (e.g. "lm_studio"), so we
    // split from the right on a known suffix.
    for suffix in ["_API_KEY", "_BASE_URL", "_MODEL"] {
        if let Some(provider) = rest.strip_suffix(suffix) {
            if !provider.is_empty() {
                return Some((provider, &suffix[1..]));
            }
        }
    }
    None
}

fn strip_env_placeholder(s: &str) -> Option<&str> {
    let s = s.trim();
    s.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_parses() {
        assert_eq!(
            "read-only".parse::<PermissionMode>().unwrap(),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            "auto-edit".parse::<PermissionMode>().unwrap(),
            PermissionMode::AutoEdit
        );
        assert_eq!(
            "yolo".parse::<PermissionMode>().unwrap(),
            PermissionMode::Yolo
        );
        assert!("nope".parse::<PermissionMode>().is_err());
    }

    #[test]
    fn env_overrides_apply() {
        let mut cfg = Config::default();
        let env = vec![
            ("ARCCODE_MODEL".to_string(), "gpt-4.1".to_string()),
            ("ARCCODE_PROVIDER".to_string(), "openai".to_string()),
            (
                "ARCCODE_PERMISSION_MODE".to_string(),
                "auto-edit".to_string(),
            ),
            (
                "ARCCODE_ANTHROPIC_API_KEY".to_string(),
                "sk-test".to_string(),
            ),
        ];
        cfg.apply_env(env).unwrap();
        assert_eq!(cfg.default_model.as_deref(), Some("gpt-4.1"));
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        assert_eq!(cfg.permission_mode, PermissionMode::AutoEdit);
        assert_eq!(
            cfg.providers
                .get("anthropic")
                .and_then(|p| p.api_key.as_deref()),
            Some("sk-test"),
        );
    }

    #[test]
    fn starter_roundtrips_through_toml() {
        let starter = Config::starter();
        let s = starter.to_toml_string().unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert_eq!(parsed.default_provider.as_deref(), Some("anthropic"));
        assert!(parsed.providers.contains_key("anthropic"));
        assert!(parsed.providers.contains_key("ollama"));
    }

    #[test]
    fn raw_merge_overlays_subtables() {
        let mut base: toml::Table = toml::from_str(
            r#"
                default_provider = "anthropic"
                [tokens]
                compact_at_tokens = 50000
                [providers.anthropic]
                model = "claude-opus-4-7"
                api_key = "from-global"
            "#,
        )
        .unwrap();
        let overlay: toml::Table = toml::from_str(
            r#"
                [providers.anthropic]
                model = "claude-sonnet-4-6"
            "#,
        )
        .unwrap();
        merge_table(&mut base, overlay);
        let cfg: Config = toml::Value::Table(base).try_into().unwrap();
        // Project file overrides model.
        assert_eq!(
            cfg.providers["anthropic"].model.as_deref(),
            Some("claude-sonnet-4-6"),
        );
        // Global api_key survives the project merge — no clobber.
        assert_eq!(
            cfg.providers["anthropic"].api_key.as_deref(),
            Some("from-global"),
        );
        // Global tokens section survives — no clobber from absent section.
        assert_eq!(cfg.tokens.compact_at_tokens, 50_000);
    }

    #[test]
    fn env_placeholder_resolved() {
        std::env::set_var("ARCCODE_TEST_KEY_42", "resolved-secret");
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".into(),
            ProviderConfig {
                api_key: Some("${ARCCODE_TEST_KEY_42}".into()),
                ..Default::default()
            },
        );
        cfg.resolve_env_placeholders();
        assert_eq!(
            cfg.providers["anthropic"].api_key.as_deref(),
            Some("resolved-secret"),
        );
        std::env::remove_var("ARCCODE_TEST_KEY_42");
    }
}
