//! Configuration loading and merging for wingman.
//!
//! Resolution order (lowest to highest precedence):
//!   1. Built-in defaults
//!   2. Global config at `~/.wingman/config.toml`
//!   3. Project config at `<project>/.wingman/config.toml`
//!   4. Environment variables (`WINGMAN_*`)
//!   5. CLI flag overrides (applied by the caller via [`Config::apply_overrides`])
//!
//! Per the plan: global `~/.wingman/` holds config/creds/model cache; per-project
//! `.wingman/` holds session log overrides and the repo index.

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
    /// Like read-only, but the assistant is expected to produce an
    /// explicit plan via `present_plan` before any write/shell tool runs.
    /// Once the user approves the plan, the runtime promotes the session
    /// to `auto-edit` for the remainder of the user turn.
    Plan,
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
            "plan" => Ok(Self::Plan),
            "auto-edit" | "autoedit" | "auto" => Ok(Self::AutoEdit),
            "yolo" => Ok(Self::Yolo),
            other => Err(format!(
                "unknown permission mode '{other}' (expected read-only, plan, auto-edit, yolo)"
            )),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReadOnly => "read-only",
            Self::Plan => "plan",
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
    /// Override the tool output budget (max lines per tool call) for this
    /// project. `None` or `0` falls back to `[tokens].tool_output_max_lines`.
    /// Resolve via [`Config::effective_tool_output_max_lines`].
    #[serde(default)]
    pub tool_output_max_lines: Option<u32>,
    /// Comma-separated list of tools to disable for this project.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Allow `web_fetch`/`web_search` in read-only/plan mode too. Off by
    /// default: network egress is otherwise gated to auto-edit/yolo so it
    /// can't be used as a data-exfiltration channel. Set true if you want
    /// look-ups while researching before you enter an edit mode.
    #[serde(default)]
    pub allow_network: bool,
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

    /// User-defined shell hooks fired at well-known points.
    #[serde(default)]
    pub hooks: HooksConfig,

    /// Periodic prompts that fire when `wingman schedule run` is invoked
    /// (e.g. from cron / a launchd plist / Task Scheduler).
    #[serde(default)]
    pub schedule: Vec<ScheduledTask>,

    /// Pilot mode (multi-agent orchestrator). See `plan.md` § Unified Pilot
    /// Mode. A legacy `[autonomous]` section is auto-migrated into `[pilot]`
    /// on load with a one-time warning.
    #[serde(default, alias = "autonomous")]
    pub pilot: PilotConfig,

    /// Post-edit verification (turn gate + receipts).
    #[serde(default)]
    pub verify: VerifyConfig,

    /// Git-native workflow (Aider-style auto-commit).
    #[serde(default)]
    pub git: GitConfig,

    /// Audit logging (compliance trail of tool calls).
    #[serde(default)]
    pub audit: AuditConfig,
}

/// Append-only audit trail of tool calls — an enterprise/compliance aid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    /// When true, every tool dispatch appends a JSONL record (timestamp, tool,
    /// a redacted input summary, error flag) to the audit log.
    pub enabled: bool,
    /// Log file path. Defaults to `<project>/.wingman/audit.log` when unset.
    pub log_path: Option<String>,
}

/// Git-native workflow options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// When true, after a turn in which the agent edited files (and the
    /// verification gate, if any, passed), auto-commit the working-tree changes
    /// with a generated message — so every AI change is a reviewable, revertable
    /// commit. Off by default. Only commits inside a git repo.
    pub auto_commit: bool,
    /// Prefix for generated commit subjects.
    pub auto_commit_prefix: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_commit: false,
            auto_commit_prefix: "wingman: ".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduledTask {
    /// Stable id used to record last-run-at.
    pub id: String,
    /// Cadence in seconds; the task fires when at least this many seconds
    /// have elapsed since its last successful run.
    pub every_secs: u64,
    /// Prompt to send headlessly.
    pub prompt: String,
    /// Optional model override (`provider/model`).
    #[serde(default)]
    pub model: Option<String>,
}

/// User-defined shell hooks. Each hook is a shell command run when the
/// matching event fires. A hook that exits non-zero with `block: true`
/// turns a tool call into an error (for `pre_tool_use`) or surfaces a
/// warning otherwise.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Fired before a tool runs. Receives `WINGMAN_TOOL_NAME` and
    /// `WINGMAN_TOOL_INPUT` (JSON) in the env.
    #[serde(default)]
    pub pre_tool_use: Vec<Hook>,
    /// Fired after a tool runs. Receives `WINGMAN_TOOL_NAME`,
    /// `WINGMAN_TOOL_INPUT`, `WINGMAN_TOOL_OUTPUT`, `WINGMAN_TOOL_IS_ERROR`.
    #[serde(default)]
    pub post_tool_use: Vec<Hook>,
    /// Fired when the assistant emits its final Stop for a user turn.
    /// Receives `WINGMAN_STOP_REASON`.
    #[serde(default)]
    pub stop: Vec<Hook>,
    /// Fired when the user submits a prompt. Receives `WINGMAN_USER_PROMPT`.
    /// If `block: true` and the hook exits non-zero, the prompt is rejected.
    #[serde(default)]
    pub user_prompt_submit: Vec<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hook {
    /// Shell command to execute. Run via `sh -c` (or `cmd /C` on Windows).
    pub command: String,
    /// Glob-ish substring match on tool name; empty = match all. Only used
    /// for tool-related hook kinds.
    #[serde(default)]
    pub match_tool: String,
    /// If true, a non-zero exit cancels the action (rejects the tool call
    /// for `pre_tool_use`, rejects the prompt for `user_prompt_submit`).
    #[serde(default)]
    pub block: bool,
    /// Timeout in seconds (default: 10).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

fn default_hook_timeout() -> u64 {
    10
}

impl Default for Hook {
    fn default() -> Self {
        Self {
            command: String::new(),
            match_tool: String::new(),
            block: false,
            timeout_secs: default_hook_timeout(),
        }
    }
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
    /// Theme name: "default" | "light" | "mono" — or any custom name that
    /// matches a `~/.wingman/themes/<name>.toml` file.
    pub theme: String,
    pub show_token_usage: bool,
    /// Optional color overrides; if any are set they override the named
    /// theme for that one role. Values are crossterm/ratatui color names
    /// (`"red"`, `"darkgray"`, …) or `"#rrggbb"` hex.
    #[serde(default)]
    pub colors: ThemeColors,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeColors {
    pub user_prompt: Option<String>,
    pub assistant: Option<String>,
    pub tool_name: Option<String>,
    pub tool_summary: Option<String>,
    pub tool_ok: Option<String>,
    pub tool_err: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    pub code_block: Option<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            show_token_usage: true,
            colors: ThemeColors::default(),
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
    /// Local model for the privacy preset — the target of the "local" class
    /// keyword. Form: `provider/model_id`, e.g. `ollama/llama3.1`. When classes
    /// like `summarize`/`compaction` map to "local", those steps never leave
    /// the machine. `wingman router preset local` prints a recommended block.
    #[serde(default)]
    pub local_model: Option<String>,
    /// Ordered fallback chain. If the primary model errors (network /
    /// rate-limit / provider 5xx), the runtime walks this list in order.
    /// Each entry is `provider/model_id`.
    #[serde(default)]
    pub fallback_models: Vec<String>,
    /// Task-class routing. Maps a task class (e.g. "search", "summarize",
    /// "codegen") to either the literal string "fast" (use `fast_model`),
    /// "default" (use the session model), or an explicit `provider/model_id`.
    /// Classes not listed here use the session model.
    ///
    /// ```toml
    /// [router.classes]
    /// search    = "fast"
    /// summarize = "fast"
    /// codegen   = "default"
    /// ```
    #[serde(default)]
    pub classes: BTreeMap<String, String>,
}

impl RouterConfig {
    /// Resolve a task class to a `provider/model_id` spec, or `None` when the
    /// session's default model should be used. Unknown classes and classes
    /// mapped to "default" return `None`; "fast" resolves through
    /// `fast_model` (and returns `None` if no fast model is configured).
    pub fn resolve_class(&self, class: &str) -> Option<String> {
        if class.is_empty() {
            return None;
        }
        match self.classes.get(class).map(String::as_str) {
            Some("fast") => self.fast_model.clone(),
            Some("local") => self.local_model.clone(),
            Some("default") | None => None,
            Some(explicit) => Some(explicit.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyConfig {
    /// Post-edit turn gate. Run after a turn in which mutating tools
    /// executed, before the agent is allowed to stop:
    /// - "auto": detect a check command from the project type
    ///   (Cargo.toml → `cargo check`, tsconfig.json → `tsc --noEmit`, …)
    /// - "off": never gate
    /// - anything else: the exact shell command to run
    pub turn_gate: String,
    /// How many gate failures are fed back to the model for self-correction
    /// before the stop is accepted anyway (with a failing receipt).
    pub max_retries: u32,
    /// After edits, also run the tests of the *changed* crates/packages
    /// (not the full suite) as part of the gate. Cargo projects only for
    /// now; a no-op elsewhere. Composes onto `turn_gate` (needs it not "off").
    pub affected_tests: bool,
    /// After edits, also fold the language server's diagnostics for the
    /// *changed* files into the gate: a turn that introduced a type error the
    /// compiler-check command didn't surface (or in a language with no cheap
    /// compile step) fails verification. Backed by whatever LSP server is on
    /// PATH; a graceful no-op (passes with a note) when none is installed.
    /// Composes onto `turn_gate` (needs it not "off").
    pub lsp_diagnostics: bool,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            turn_gate: "auto".into(),
            max_retries: 2,
            affected_tests: true,
            lsp_diagnostics: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing-subscriber` env-filter directive.
    pub filter: String,
    /// Write logs to a file under `~/.wingman/logs/`.
    pub file: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "info,wingman=info".into(),
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
    /// Environment variables for the stdio child process. Most MCP servers
    /// take their API key / config via env, so this is required to reach them.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Working directory for the stdio child process.
    #[serde(default)]
    pub cwd: Option<String>,
    /// URL for http transport.
    pub url: Option<String>,
    /// Extra HTTP headers for http transport (e.g. `Authorization`). Needed to
    /// reach authenticated remote MCP servers.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Whether this server's tools are trusted to run in read-only/plan mode.
    /// MCP tools are opaque — we can't tell a safe search tool from one that
    /// writes files or runs commands — so by default they are gated to
    /// edit-capable modes (auto-edit/yolo) just like the shell tool. Set this
    /// true only for servers you know are side-effect-free.
    #[serde(default)]
    pub trusted: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".into(),
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            url: None,
            headers: std::collections::BTreeMap::new(),
            trusted: false,
        }
    }
}

impl Config {
    /// Effective per-tool output line budget: the `[tools]` project override
    /// when set to a non-zero value, else the global `[tokens]` default.
    pub fn effective_tool_output_max_lines(&self) -> u32 {
        match self.tools.tool_output_max_lines {
            Some(n) if n > 0 => n,
            _ => self.tokens.tool_output_max_lines,
        }
    }

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

    /// Apply `WINGMAN_*` environment variables.
    ///
    /// Currently supported:
    ///   - `WINGMAN_MODEL`            -> `default_model`
    ///   - `WINGMAN_PROVIDER`         -> `default_provider`
    ///   - `WINGMAN_PERMISSION_MODE`  -> `permission_mode`
    ///   - `WINGMAN_LOG`              -> `logging.filter`
    ///   - `WINGMAN_<PROVIDER>_API_KEY`  -> providers[<provider>].api_key
    ///   - `WINGMAN_<PROVIDER>_BASE_URL` -> providers[<provider>].base_url
    pub fn apply_env<I>(&mut self, vars: I) -> Result<(), ConfigError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        for (k, v) in vars {
            if !k.starts_with("WINGMAN_") {
                continue;
            }
            match k.as_str() {
                "WINGMAN_MODEL" => self.default_model = Some(v),
                "WINGMAN_PROVIDER" => self.default_provider = Some(v),
                "WINGMAN_PERMISSION_MODE" => {
                    self.permission_mode = v.parse().map_err(|e: String| ConfigError::BadEnv {
                        name: k.clone(),
                        value: v.clone(),
                        reason: e,
                    })?;
                }
                "WINGMAN_LOG" => self.logging.filter = v,
                _ => {
                    if let Some(rest) = k.strip_prefix("WINGMAN_") {
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
        // The webhook HMAC secret supports the same `${ENV_VAR}` indirection so
        // it need not be stored in plaintext config.
        if let Some(s) = self.pilot.daemon.webhook_secret.as_mut() {
            if let Some(name) = strip_env_placeholder(s) {
                if let Ok(val) = std::env::var(name) {
                    *s = val;
                }
            }
        }
        // Notification webhook URLs are secrets too — resolve `${ENV_VAR}`.
        for url in self.pilot.notifications.webhooks.values_mut() {
            if let Some(name) = strip_env_placeholder(url) {
                if let Ok(val) = std::env::var(name) {
                    *url = val;
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
        // The config may carry a plaintext api_key (keyring-unavailable
        // fallback), so lock it to owner-only before it becomes visible under
        // the final name. Set on the temp file to avoid a world-readable window.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(
                |source| ConfigError::Io {
                    path: tmp.clone(),
                    source,
                },
            )?;
        }
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
        Self::write_provider_layer(path, provider_id, model, base_url, None, with_keyring, true)
    }

    /// Like above but also stores a plaintext api_key in the config layer
    /// (used when the keyring is unavailable or when the caller wants to
    /// skip the keyring for speed).
    pub fn set_default_provider_and_save_with_key(
        path: &Path,
        provider_id: &str,
        model: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<(), ConfigError> {
        Self::write_provider_layer(path, provider_id, model, base_url, api_key, false, true)
    }

    /// Persist a single provider's model / base URL / keyring marker to the
    /// config file *without* changing the default provider or model. Used by
    /// `wingman login --no-default` to register an additional provider while
    /// leaving the current default selection untouched.
    pub fn set_provider_and_save(
        path: &Path,
        provider_id: &str,
        model: &str,
        base_url: Option<&str>,
        with_keyring: bool,
    ) -> Result<(), ConfigError> {
        Self::write_provider_layer(
            path,
            provider_id,
            model,
            base_url,
            None,
            with_keyring,
            false,
        )
    }

    /// Shared implementation for the two `*_provider_and_save` entry points.
    /// Edits only this one config layer (re-reads the raw file, not the merged
    /// config) and optionally promotes the provider to the default.
    fn write_provider_layer(
        path: &Path,
        provider_id: &str,
        model: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
        with_keyring: bool,
        set_default: bool,
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

        if set_default {
            cfg.default_provider = Some(provider_id.to_string());
            cfg.default_model = Some(format!("{provider_id}/{model}"));
        }

        let entry = cfg.providers.entry(provider_id.to_string()).or_default();
        entry.model = Some(model.to_string());
        if let Some(url) = base_url {
            entry.base_url = Some(url.to_string());
        }
        if let Some(key) = api_key {
            entry.api_key = Some(key.to_string());
        } else if with_keyring {
            entry.api_key = Some(format!("keyring:{provider_id}"));
        }

        cfg.save_atomic(path)
    }

    /// Starter config written by `wingman config init`.
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
                "chatgpt".to_string(),
                ProviderConfig {
                    // Token stored in keychain after `wingman login chatgpt
                    // --oauth` / the /login wizard OAuth flow. Set via
                    // CHATGPT_ACCESS_TOKEN env var as an alternative.
                    api_key: Some("${CHATGPT_ACCESS_TOKEN}".into()),
                    model: Some("gpt-4o".into()),
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
            (
                "groq".to_string(),
                ProviderConfig {
                    api_key: Some("${GROQ_API_KEY}".into()),
                    model: Some("llama-3.3-70b-versatile".into()),
                    ..Default::default()
                },
            ),
            (
                "together".to_string(),
                ProviderConfig {
                    api_key: Some("${TOGETHER_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".into()),
                    ..Default::default()
                },
            ),
            (
                "fireworks".to_string(),
                ProviderConfig {
                    api_key: Some("${FIREWORKS_API_KEY}".into()),
                    model: Some("accounts/fireworks/models/llama-v3p1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "deepinfra".to_string(),
                ProviderConfig {
                    api_key: Some("${DEEPINFRA_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "perplexity".to_string(),
                ProviderConfig {
                    api_key: Some("${PERPLEXITY_API_KEY}".into()),
                    model: Some("sonar-pro".into()),
                    ..Default::default()
                },
            ),
            (
                "xai".to_string(),
                ProviderConfig {
                    api_key: Some("${XAI_API_KEY}".into()),
                    model: Some("grok-2-latest".into()),
                    ..Default::default()
                },
            ),
            (
                "deepseek".to_string(),
                ProviderConfig {
                    api_key: Some("${DEEPSEEK_API_KEY}".into()),
                    model: Some("deepseek-chat".into()),
                    ..Default::default()
                },
            ),
            (
                "mistral".to_string(),
                ProviderConfig {
                    api_key: Some("${MISTRAL_API_KEY}".into()),
                    model: Some("mistral-large-latest".into()),
                    ..Default::default()
                },
            ),
            (
                "cerebras".to_string(),
                ProviderConfig {
                    api_key: Some("${CEREBRAS_API_KEY}".into()),
                    model: Some("llama3.1-70b".into()),
                    ..Default::default()
                },
            ),
            (
                "sambanova".to_string(),
                ProviderConfig {
                    api_key: Some("${SAMBANOVA_API_KEY}".into()),
                    model: Some("Meta-Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "azure".to_string(),
                ProviderConfig {
                    api_key: Some("${AZURE_OPENAI_API_KEY}".into()),
                    // Azure requires a per-deployment URL; user must edit.
                    base_url: Some(
                        "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT"
                            .into(),
                    ),
                    model: Some("gpt-4o".into()),
                    ..Default::default()
                },
            ),
            (
                "github".to_string(),
                ProviderConfig {
                    api_key: Some("${GITHUB_TOKEN}".into()),
                    model: Some("gpt-4o".into()),
                    ..Default::default()
                },
            ),
            (
                "llamacpp".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:8080/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "tgi".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:3000/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "anyscale".to_string(),
                ProviderConfig {
                    api_key: Some("${ANYSCALE_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "lepton".to_string(),
                ProviderConfig {
                    api_key: Some("${LEPTON_API_KEY}".into()),
                    model: Some("llama3-1-70b".into()),
                    ..Default::default()
                },
            ),
            (
                "replicate".to_string(),
                ProviderConfig {
                    api_key: Some("${REPLICATE_API_TOKEN}".into()),
                    model: Some("meta/meta-llama-3.1-405b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "novita".to_string(),
                ProviderConfig {
                    api_key: Some("${NOVITA_API_KEY}".into()),
                    model: Some("meta-llama/llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "hyperbolic".to_string(),
                ProviderConfig {
                    api_key: Some("${HYPERBOLIC_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "lambda".to_string(),
                ProviderConfig {
                    api_key: Some("${LAMBDA_API_KEY}".into()),
                    model: Some("llama3.1-70b-instruct-fp8".into()),
                    ..Default::default()
                },
            ),
            (
                "nebius".to_string(),
                ProviderConfig {
                    api_key: Some("${NEBIUS_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-70B-Instruct-fast".into()),
                    ..Default::default()
                },
            ),
            (
                "hf".to_string(),
                ProviderConfig {
                    api_key: Some("${HF_TOKEN}".into()),
                    model: Some("meta-llama/Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "glhf".to_string(),
                ProviderConfig {
                    api_key: Some("${GLHF_API_KEY}".into()),
                    model: Some("hf:meta-llama/Llama-3.1-70B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "featherless".to_string(),
                ProviderConfig {
                    api_key: Some("${FEATHERLESS_API_KEY}".into()),
                    model: Some("meta-llama/Meta-Llama-3.1-8B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "octoai".to_string(),
                ProviderConfig {
                    api_key: Some("${OCTOAI_API_KEY}".into()),
                    model: Some("meta-llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "nvidia".to_string(),
                ProviderConfig {
                    api_key: Some("${NVIDIA_API_KEY}".into()),
                    model: Some("meta/llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "avian".to_string(),
                ProviderConfig {
                    api_key: Some("${AVIAN_API_KEY}".into()),
                    model: Some("Meta-Llama-3.1-405B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "kluster".to_string(),
                ProviderConfig {
                    api_key: Some("${KLUSTER_API_KEY}".into()),
                    model: Some("klusterai/Meta-Llama-3.1-405B-Instruct-Turbo".into()),
                    ..Default::default()
                },
            ),
            (
                "inferencenet".to_string(),
                ProviderConfig {
                    api_key: Some("${INFERENCE_NET_API_KEY}".into()),
                    model: Some("meta-llama/llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "snowflake".to_string(),
                ProviderConfig {
                    api_key: Some("${SNOWFLAKE_API_KEY}".into()),
                    base_url: Some(
                        "https://YOUR-ACCOUNT.snowflakecomputing.com/api/v2/cortex/inference/v1"
                            .into(),
                    ),
                    model: Some("llama3.1-70b".into()),
                    ..Default::default()
                },
            ),
            (
                "databricks".to_string(),
                ProviderConfig {
                    api_key: Some("${DATABRICKS_TOKEN}".into()),
                    base_url: Some(
                        "https://YOUR-WORKSPACE.cloud.databricks.com/serving-endpoints/v1".into(),
                    ),
                    model: Some("databricks-meta-llama-3-1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "writer".to_string(),
                ProviderConfig {
                    api_key: Some("${WRITER_API_KEY}".into()),
                    model: Some("palmyra-x5".into()),
                    ..Default::default()
                },
            ),
            (
                "cohere".to_string(),
                ProviderConfig {
                    api_key: Some("${COHERE_API_KEY}".into()),
                    model: Some("command-r-plus".into()),
                    ..Default::default()
                },
            ),
            (
                "gpt4all".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:4891/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "jan".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:1337/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "koboldcpp".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:5001/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "oobabooga".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:5000/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "qwen".to_string(),
                ProviderConfig {
                    api_key: Some("${DASHSCOPE_API_KEY}".into()),
                    model: Some("qwen-max".into()),
                    ..Default::default()
                },
            ),
            (
                "zhipu".to_string(),
                ProviderConfig {
                    api_key: Some("${ZHIPU_API_KEY}".into()),
                    model: Some("glm-4-plus".into()),
                    ..Default::default()
                },
            ),
            (
                "moonshot".to_string(),
                ProviderConfig {
                    api_key: Some("${MOONSHOT_API_KEY}".into()),
                    model: Some("moonshot-v1-128k".into()),
                    ..Default::default()
                },
            ),
            (
                "minimax".to_string(),
                ProviderConfig {
                    api_key: Some("${MINIMAX_API_KEY}".into()),
                    model: Some("abab6.5s-chat".into()),
                    ..Default::default()
                },
            ),
            (
                "yi".to_string(),
                ProviderConfig {
                    api_key: Some("${YI_API_KEY}".into()),
                    model: Some("yi-large".into()),
                    ..Default::default()
                },
            ),
            (
                "baichuan".to_string(),
                ProviderConfig {
                    api_key: Some("${BAICHUAN_API_KEY}".into()),
                    model: Some("Baichuan4-Turbo".into()),
                    ..Default::default()
                },
            ),
            (
                "hunyuan".to_string(),
                ProviderConfig {
                    api_key: Some("${HUNYUAN_API_KEY}".into()),
                    model: Some("hunyuan-pro".into()),
                    ..Default::default()
                },
            ),
            (
                "doubao".to_string(),
                ProviderConfig {
                    api_key: Some("${ARK_API_KEY}".into()),
                    model: Some("doubao-pro-32k".into()),
                    ..Default::default()
                },
            ),
            (
                "siliconflow".to_string(),
                ProviderConfig {
                    api_key: Some("${SILICONFLOW_API_KEY}".into()),
                    model: Some("Qwen/Qwen2.5-72B-Instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "cloudflare".to_string(),
                ProviderConfig {
                    api_key: Some("${CLOUDFLARE_API_TOKEN}".into()),
                    base_url: Some(
                        "https://api.cloudflare.com/client/v4/accounts/YOUR-ACCOUNT-ID/ai/v1"
                            .into(),
                    ),
                    model: Some("@cf/meta/llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "vercel".to_string(),
                ProviderConfig {
                    api_key: Some("${VERCEL_AI_GATEWAY_KEY}".into()),
                    model: Some("openai/gpt-4o".into()),
                    ..Default::default()
                },
            ),
            (
                "aimlapi".to_string(),
                ProviderConfig {
                    api_key: Some("${AIMLAPI_KEY}".into()),
                    model: Some("meta-llama/Llama-3.3-70B-Instruct-Turbo".into()),
                    ..Default::default()
                },
            ),
            (
                "openpipe".to_string(),
                ProviderConfig {
                    api_key: Some("${OPENPIPE_API_KEY}".into()),
                    model: Some("openpipe:meta-llama-3.1-70b".into()),
                    ..Default::default()
                },
            ),
            (
                "targon".to_string(),
                ProviderConfig {
                    api_key: Some("${TARGON_API_KEY}".into()),
                    model: Some("NousResearch/Hermes-3-Llama-3.1-70B".into()),
                    ..Default::default()
                },
            ),
            (
                "pollinations".to_string(),
                ProviderConfig {
                    model: Some("openai".into()),
                    ..Default::default()
                },
            ),
            (
                "ai21".to_string(),
                ProviderConfig {
                    api_key: Some("${AI21_API_KEY}".into()),
                    model: Some("jamba-1.5-large".into()),
                    ..Default::default()
                },
            ),
            (
                "zai".to_string(),
                ProviderConfig {
                    api_key: Some("${ZAI_API_KEY}".into()),
                    model: Some("glm-4-plus".into()),
                    ..Default::default()
                },
            ),
            (
                "friendli".to_string(),
                ProviderConfig {
                    api_key: Some("${FRIENDLI_TOKEN}".into()),
                    model: Some("meta-llama-3.1-70b-instruct".into()),
                    ..Default::default()
                },
            ),
            (
                "mancer".to_string(),
                ProviderConfig {
                    api_key: Some("${MANCER_API_KEY}".into()),
                    model: Some("weaver".into()),
                    ..Default::default()
                },
            ),
            (
                "reka".to_string(),
                ProviderConfig {
                    api_key: Some("${REKA_API_KEY}".into()),
                    model: Some("reka-core".into()),
                    ..Default::default()
                },
            ),
            (
                "mlx".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:8080/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "localai".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:8080/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "aphrodite".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:2242/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "mistralrs".to_string(),
                ProviderConfig {
                    base_url: Some("http://localhost:1234/v1".into()),
                    model: Some("local-model".into()),
                    ..Default::default()
                },
            ),
            (
                "bedrock".to_string(),
                ProviderConfig {
                    // Long-term Bedrock API key. Generate from AWS console
                    // (Bedrock → API keys). For SigV4 auth, leave this and
                    // rely on standard AWS env vars / shared config.
                    api_key: Some("${AWS_BEARER_TOKEN_BEDROCK}".into()),
                    // Region must match the bearer token's region.
                    base_url: Some(
                        "https://bedrock-runtime.us-east-1.amazonaws.com/openai/v1".into(),
                    ),
                    model: Some(
                        "us.anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
                    ),
                    ..Default::default()
                },
            ),
            (
                "vertex".to_string(),
                ProviderConfig {
                    // Short-lived access token; refresh with
                    //   gcloud auth print-access-token
                    api_key: Some("${GOOGLE_VERTEX_TOKEN}".into()),
                    base_url: Some(
                        "https://us-central1-aiplatform.googleapis.com/v1/projects/YOUR-PROJECT/locations/us-central1/endpoints/openapi".into(),
                    ),
                    model: Some("google/gemini-1.5-pro-002".into()),
                    ..Default::default()
                },
            ),
            (
                "watsonx".to_string(),
                ProviderConfig {
                    // IBM Cloud API key — adapter exchanges it for an IAM
                    // access token automatically. To use a pre-obtained
                    // token instead, set WATSONX_ACCESS_TOKEN in env.
                    api_key: Some("${WATSONX_API_KEY}".into()),
                    base_url: Some("https://us-south.ml.cloud.ibm.com".into()),
                    model: Some("ibm/granite-3-8b-instruct".into()),
                    // project_id is required and must be set out-of-band
                    // (via `[providers.watsonx] project_id = "…"` in the
                    // config or `WATSONX_PROJECT_ID` env var).
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
    // WINGMAN_<PROVIDER>_<FIELD> where FIELD is one of API_KEY, BASE_URL, MODEL.
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

/// Capability tier — which pilot-mode features are on by default.
///
/// `assist` keeps the user in the loop on every decision; `copilot` is the
/// default for day-to-day work; `autopilot` enables daemon discovery, the
/// critic agent, and sandboxed execution. See `plan.md` § Capability tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PilotTier {
    Assist,
    #[default]
    Copilot,
    Autopilot,
}

impl std::str::FromStr for PilotTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "assist" => Ok(Self::Assist),
            "copilot" => Ok(Self::Copilot),
            "autopilot" => Ok(Self::Autopilot),
            other => Err(format!(
                "unknown pilot tier '{other}' (expected assist, copilot, autopilot)"
            )),
        }
    }
}

impl std::fmt::Display for PilotTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Assist => "assist",
            Self::Copilot => "copilot",
            Self::Autopilot => "autopilot",
        })
    }
}

/// Top-level pilot-mode settings. See `plan.md` § Unified config schema.
///
/// Defaults mirror the table in the plan: `copilot` tier, 4-way concurrency,
/// $10 budget, 30-minute task timeout, `cargo check --workspace` as the
/// per-turn gate (E5).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotConfig {
    pub tier: PilotTier,
    /// Model used for the manager agent, reviewers, and the critic. Form:
    /// `provider/model_id` (e.g. `anthropic/claude-opus-4-7`).
    pub default_model: Option<String>,
    /// Cheaper model used for worker subprocesses.
    pub worker_model: Option<String>,
    /// Model for the per-task reviewer / critic. Defaults to `default_model`
    /// when unset — point it at a stronger model for tougher review.
    #[serde(default)]
    pub reviewer_model: Option<String>,
    pub max_concurrent_agents: u32,
    pub max_usd: f64,
    pub task_timeout_secs: u64,
    /// Shell command run between worker turns as a sanity gate (E5).
    /// Empty disables the per-turn check.
    pub turn_gate_cmd: String,

    pub approval: PilotApprovalConfig,
    pub pr: PilotPrConfig,
    pub sandbox: PilotSandboxConfig,
    pub daemon: PilotDaemonConfig,
    pub refine: PilotRefineConfig,
    pub skills: PilotSkillsConfig,
    pub security: PilotSecurityConfig,
    pub notifications: PilotNotificationsConfig,

    /// Per-capability overrides. Each key turns one E1–E13 / J1–J15
    /// capability on or off regardless of the tier's defaults.
    #[serde(default)]
    pub capabilities: BTreeMap<String, bool>,
}

impl Default for PilotConfig {
    fn default() -> Self {
        Self {
            tier: PilotTier::default(),
            default_model: None,
            worker_model: None,
            reviewer_model: None,
            max_concurrent_agents: 4,
            max_usd: 10.0,
            task_timeout_secs: 1800,
            turn_gate_cmd: "cargo check --workspace".into(),
            approval: PilotApprovalConfig::default(),
            pr: PilotPrConfig::default(),
            sandbox: PilotSandboxConfig::default(),
            daemon: PilotDaemonConfig::default(),
            refine: PilotRefineConfig::default(),
            skills: PilotSkillsConfig::default(),
            security: PilotSecurityConfig::default(),
            notifications: PilotNotificationsConfig::default(),
            capabilities: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotApprovalConfig {
    pub auto_approve_usd: f64,
    pub auto_approve_max_tasks: u32,
    pub auto_approve_globs: Vec<String>,
    /// Plans touching these globs always require a hard approval gate.
    pub dangerous_paths: Vec<String>,
    /// "Veto in N seconds" window for medium-risk plans.
    pub notify_only_window_secs: u64,
    /// Where notify-only plans are surfaced (e.g. "desktop", "slack:<webhook>").
    pub notify_channel: String,
}

impl Default for PilotApprovalConfig {
    fn default() -> Self {
        Self {
            auto_approve_usd: 1.00,
            auto_approve_max_tasks: 5,
            auto_approve_globs: vec![
                "crates/**/*.rs".into(),
                "docs/**".into(),
                "README.md".into(),
            ],
            dangerous_paths: vec![
                "**/migrations/**".into(),
                ".github/**".into(),
                "**/auth/**".into(),
                "**/secrets*".into(),
                "Cargo.lock".into(),
            ],
            notify_only_window_secs: 60,
            notify_channel: "desktop".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotPrConfig {
    pub auto_merge: bool,
    /// "low" | "medium" | "high" — auto-merge is vetoed if `wingman review`
    /// turns up any finding at or above this severity.
    pub auto_merge_max_severity: String,
    pub require_ci_green: bool,
    /// Branch the pilot opens its PR against. Defaults to `main`; set this to
    /// your repo's default branch (e.g. `master`). The
    /// `WINGMAN_PILOT_BASE_BRANCH` env var overrides it for one-off runs.
    pub base_branch: String,
    /// Severity at/above which the per-task reviewer sends work back for
    /// rework: "low" | "medium" | "high" | "critical". Defaults to `high` —
    /// acceptance checks already gate functional correctness before review, so
    /// an over-eager reviewer model can't loop a correct change. Lower it for
    /// stricter review with a well-calibrated reviewer model.
    pub reviewer_rework_severity: String,
}

impl Default for PilotPrConfig {
    fn default() -> Self {
        Self {
            auto_merge: true,
            auto_merge_max_severity: "low".into(),
            require_ci_green: true,
            base_branch: "main".into(),
            reviewer_rework_severity: "high".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotSandboxConfig {
    /// "host" | "container" | "vm" — where workers run by default.
    pub default_tier: String,
    pub container_image: String,
    /// "firecracker" | "qemu" | "cloud".
    pub vm_provider: String,
    /// Fail-closed switch for the untrusted/irreversible ("vm") tier.
    /// Real sandboxed worker execution isn't wired yet, so by default pilot
    /// *refuses* to run a vm-tier task (migrations, infra, irreversible, or
    /// untrusted goals) rather than silently executing it unsandboxed on the
    /// host. Set to true to accept host execution for those tasks.
    pub allow_unsandboxed_vm_tasks: bool,
}

impl Default for PilotSandboxConfig {
    fn default() -> Self {
        Self {
            default_tier: "host".into(),
            container_image: "wingman/sandbox:latest".into(),
            vm_provider: "firecracker".into(),
            allow_unsandboxed_vm_tasks: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotDaemonConfig {
    pub enabled: bool,
    pub poll_interval_secs: u64,
    pub auto_threshold: f64,
    pub max_concurrent_runs: u32,
    pub trusted_authors: Vec<String>,
    pub trusted_labels: Vec<String>,
    pub sources: Vec<String>,
    /// HMAC-SHA256 shared secret for the inbound J3 webhook. When set, every
    /// `POST /goals` must carry a valid `X-Wingman-Signature: sha256=<hex>`
    /// header over the body or it's rejected with 401, and only then may a
    /// body-claimed author be honored for trust. Empty/unset disables the
    /// webhook's trust elevation (claimed authors stay anonymous). May be a
    /// `${ENV_VAR}` placeholder so the secret isn't stored in plaintext.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// J2 — when true, a candidate the daemon scores as `AutoRun` is
    /// dispatched into a real nested pilot run (plans, spawns workers, opens
    /// a PR) instead of only being queued. Default false so enabling the
    /// daemon surfaces work without silently opening PRs; flip it on once the
    /// trust config (`trusted_authors`/`trusted_labels`, `auto_threshold`) is
    /// tuned.
    #[serde(default)]
    pub auto_dispatch: bool,
    /// J3 file-drop intake directory (relative to the repo root). When the
    /// `intake` source is enabled, each `*.md` here is normalized into a goal
    /// candidate and flows through the same score/dispatch path as discovered
    /// work. A Slack/email gateway that writes messages into this directory is
    /// the "transport"; wingman consumes it, so no in-process listener is
    /// needed. An optional first line `author: <name>` sets trust.
    #[serde(default = "default_intake_dir")]
    pub intake_dir: String,
}

fn default_intake_dir() -> String {
    ".wingman/intake".into()
}

impl Default for PilotDaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: 300,
            auto_threshold: 0.75,
            max_concurrent_runs: 2,
            trusted_authors: Vec::new(),
            trusted_labels: vec!["wingman:auto".into()],
            auto_dispatch: false,
            // Live sources: github_issues, todos, ci_failures, dependabot,
            // coverage_gaps, intake. The default advertises only
            // `github_issues`; add the others explicitly.
            sources: vec!["github_issues".into()],
            webhook_secret: None,
            intake_dir: default_intake_dir(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotRefineConfig {
    /// Cap on clarifying questions the agent may ask before planning (J1).
    pub max_clarifying_questions: u32,
    /// "off" | "low" | "medium" | "high" — how aggressively the agent
    /// challenges goals it thinks are wrong.
    pub challenge_threshold: String,
    pub suggest_alternatives: bool,
}

impl Default for PilotRefineConfig {
    fn default() -> Self {
        Self {
            max_clarifying_questions: 3,
            challenge_threshold: "medium".into(),
            suggest_alternatives: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotSkillsConfig {
    /// Installed skill packs, each `owner/name@semver`.
    pub packs: Vec<String>,
}

/// R6 — security pass run before E8's auto-merge gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotSecurityConfig {
    /// Secrets scanner binary to invoke on the diff (e.g. "gitleaks").
    /// Empty disables the external scanner (the built-in heuristic scan
    /// still runs).
    pub secrets_scanner: String,
    /// Run `cargo audit` / `npm audit` on lockfile changes.
    pub dependency_audit: bool,
    /// SPDX identifiers permitted for new dependencies.
    pub allowed_licenses: Vec<String>,
    /// Findings at or above this severity block auto-merge.
    /// "info" | "low" | "medium" | "high" | "critical".
    pub block_severity: String,
}

/// R5 — notification routing & digesting. Each severity tier routes to a
/// set of channels, or the special sinks "digest" (batched) / "suppress".
/// `escalation` / `decision` are channel lists (immediate); `progress` /
/// `info` are single tokens that may be a channel, "digest", or
/// "suppress".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PilotNotificationsConfig {
    pub escalation: Vec<String>,
    pub decision: Vec<String>,
    pub progress: String,
    pub info: String,
    /// Cron expression for flushing the digest queue.
    pub digest_cron: String,
    /// Delivery endpoints per channel name: `channel -> webhook URL`. A
    /// routed channel with an entry here is POSTed a `{"text": ...}` payload
    /// (the Slack incoming-webhook shape; also works for Discord/Teams/generic
    /// receivers and email-webhook services). Channels without an entry fall
    /// back to the terminal. Values support `${ENV_VAR}` so the URL (a secret)
    /// can come from the environment.
    #[serde(default)]
    pub webhooks: BTreeMap<String, String>,
}

impl Default for PilotNotificationsConfig {
    fn default() -> Self {
        Self {
            escalation: vec!["desktop".into(), "slack".into(), "email".into()],
            decision: vec!["desktop".into(), "slack".into()],
            progress: "digest".into(),
            info: "suppress".into(),
            digest_cron: "0 9 * * *".into(),
            webhooks: BTreeMap::new(),
        }
    }
}

impl Default for PilotSecurityConfig {
    fn default() -> Self {
        Self {
            secrets_scanner: "gitleaks".into(),
            dependency_audit: true,
            allowed_licenses: vec![
                "MIT".into(),
                "Apache-2.0".into(),
                "BSD-3-Clause".into(),
                "BSD-2-Clause".into(),
                "ISC".into(),
                "MPL-2.0".into(),
                "Unicode-DFS-2016".into(),
            ],
            block_severity: "medium".into(),
        }
    }
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
    fn mcp_server_parses_env_headers_cwd_trusted() {
        let cfg: Config = toml::from_str(
            r#"
            [mcp.fs]
            transport = "stdio"
            command = "mcp-fs"
            cwd = "/srv/proj"
            trusted = true
            env = { API_KEY = "secret", DEBUG = "1" }

            [mcp.remote]
            transport = "http"
            url = "https://mcp.example.com/mcp"
            headers = { Authorization = "Bearer abc" }
            "#,
        )
        .unwrap();
        let fs = &cfg.mcp["fs"];
        assert_eq!(fs.cwd.as_deref(), Some("/srv/proj"));
        assert!(fs.trusted);
        assert_eq!(fs.env["API_KEY"], "secret");
        let remote = &cfg.mcp["remote"];
        assert!(!remote.trusted, "trusted defaults to false");
        assert_eq!(remote.headers["Authorization"], "Bearer abc");
    }

    #[test]
    fn env_overrides_apply() {
        let mut cfg = Config::default();
        let env = vec![
            ("WINGMAN_MODEL".to_string(), "gpt-4.1".to_string()),
            ("WINGMAN_PROVIDER".to_string(), "openai".to_string()),
            (
                "WINGMAN_PERMISSION_MODE".to_string(),
                "auto-edit".to_string(),
            ),
            (
                "WINGMAN_ANTHROPIC_API_KEY".to_string(),
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
    fn set_provider_variants_respect_default_flag() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let path = dir.join(format!("wingman-cfg-test-{pid}.toml"));
        let _ = std::fs::remove_file(&path);

        // Seed an existing default, then register a second provider without
        // promoting it — the default must be untouched, the section present.
        Config::set_default_provider_and_save(&path, "anthropic", "claude-opus-4-7", None, true)
            .unwrap();
        Config::set_provider_and_save(&path, "openai", "gpt-4.1", None, true).unwrap();

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("anthropic/claude-opus-4-7")
        );
        assert_eq!(
            cfg.providers.get("openai").and_then(|p| p.model.as_deref()),
            Some("gpt-4.1")
        );
        assert_eq!(
            cfg.providers
                .get("openai")
                .and_then(|p| p.api_key.as_deref()),
            Some("keyring:openai")
        );

        // Now promote openai — default flips, anthropic section remains.
        Config::set_default_provider_and_save(&path, "openai", "gpt-4.1", None, true).unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        assert!(cfg.providers.contains_key("anthropic"));

        let _ = std::fs::remove_file(&path);
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
    fn pilot_config_defaults() {
        let cfg = PilotConfig::default();
        assert_eq!(cfg.tier, PilotTier::Copilot);
        assert_eq!(cfg.max_concurrent_agents, 4);
        assert!((cfg.max_usd - 10.0).abs() < 1e-9);
        assert_eq!(cfg.task_timeout_secs, 1800);
        assert_eq!(cfg.turn_gate_cmd, "cargo check --workspace");
        assert!(cfg.pr.auto_merge);
        assert_eq!(cfg.sandbox.default_tier, "host");
        assert!(!cfg.daemon.enabled);
    }

    #[test]
    fn pilot_tier_parses() {
        assert_eq!("assist".parse::<PilotTier>().unwrap(), PilotTier::Assist);
        assert_eq!("copilot".parse::<PilotTier>().unwrap(), PilotTier::Copilot);
        assert_eq!(
            "autopilot".parse::<PilotTier>().unwrap(),
            PilotTier::Autopilot
        );
        assert!("orbit".parse::<PilotTier>().is_err());
    }

    #[test]
    fn legacy_autonomous_section_migrates_to_pilot() {
        // Per plan.md § Migration: existing [autonomous] config should be
        // honored as [pilot] until M4 removes the alias.
        let text = r#"
            [autonomous]
            tier = "assist"
            max_concurrent_agents = 2
            max_usd = 5.0
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.pilot.tier, PilotTier::Assist);
        assert_eq!(cfg.pilot.max_concurrent_agents, 2);
        assert!((cfg.pilot.max_usd - 5.0).abs() < 1e-9);
    }

    #[test]
    fn env_placeholder_resolved() {
        std::env::set_var("WINGMAN_TEST_KEY_42", "resolved-secret");
        let mut cfg = Config::default();
        cfg.providers.insert(
            "anthropic".into(),
            ProviderConfig {
                api_key: Some("${WINGMAN_TEST_KEY_42}".into()),
                ..Default::default()
            },
        );
        cfg.resolve_env_placeholders();
        assert_eq!(
            cfg.providers["anthropic"].api_key.as_deref(),
            Some("resolved-secret"),
        );
        std::env::remove_var("WINGMAN_TEST_KEY_42");
    }

    #[test]
    fn router_resolve_class() {
        let text = r#"
            [router]
            fast_model = "anthropic/claude-haiku-4-5-20251001"

            [router.classes]
            search    = "fast"
            summarize = "fast"
            codegen   = "default"
            review    = "openrouter/qwen-coder"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        let r = &cfg.router;
        assert_eq!(
            r.resolve_class("search").as_deref(),
            Some("anthropic/claude-haiku-4-5-20251001")
        );
        assert_eq!(
            r.resolve_class("review").as_deref(),
            Some("openrouter/qwen-coder")
        );
        // "default", unknown classes, and the empty class use the session model.
        assert_eq!(r.resolve_class("codegen"), None);
        assert_eq!(r.resolve_class("reason"), None);
        assert_eq!(r.resolve_class(""), None);
    }

    #[test]
    fn router_class_fast_without_fast_model_falls_back() {
        let text = r#"
            [router.classes]
            search = "fast"
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.router.resolve_class("search"), None);
    }

    #[test]
    fn verify_config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.verify.turn_gate, "auto");
        assert_eq!(cfg.verify.max_retries, 2);

        let text = r#"
            [verify]
            turn_gate = "off"
            max_retries = 1
        "#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.verify.turn_gate, "off");
        assert_eq!(cfg.verify.max_retries, 1);
    }
}
