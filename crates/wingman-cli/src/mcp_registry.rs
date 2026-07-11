//! Runtime management of MCP servers.
//!
//! Wraps the per-server [`wingman_mcp::McpServer`] handles + their currently
//! registered tool names so we can connect/disconnect/add/remove from the
//! TUI without restarting the agent.
//!
//! The registry holds a shared [`Arc<ToolRegistry>`] — the same one the
//! agent loop is using — and reaches in via the interior-mutable
//! `register_arc` / `unregister` methods.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use wingman_config::{global_config_path, Config, McpServerConfig};
use wingman_mcp::{McpServer, McpTool, McpToolHandle};
use wingman_tools::ToolRegistry;
use tokio::sync::Mutex;

use crate::mcp_adapter::McpToolAdapter;

/// Public-facing summary of one server. Returned by [`McpRegistry::list`]
/// so the TUI doesn't need to know about internal types.
#[derive(Debug, Clone)]
pub struct McpServerView {
    pub name: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub connected: bool,
    pub tool_names: Vec<String>,
}

struct Entry {
    config: McpServerConfig,
    /// Present when the server is connected.
    server: Option<McpServer>,
    /// Tool names registered into `ToolRegistry` for this server. Tracked
    /// here so we can cleanly deregister on disconnect/remove.
    tool_names: Vec<String>,
}

pub struct McpRegistry {
    tools: Arc<ToolRegistry>,
    config_path: PathBuf,
    inner: Mutex<BTreeMap<String, Entry>>,
}

impl McpRegistry {
    pub fn new(tools: Arc<ToolRegistry>) -> Self {
        let config_path = global_config_path().unwrap_or_default();
        Self {
            tools,
            config_path,
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Switch the permission mode on the shared `ToolRegistry` — the same
    /// one the running agent dispatches through, so `/mode` re-gates tools
    /// mid-session.
    pub fn set_mode(&self, mode: wingman_config::PermissionMode) {
        self.tools.set_mode(mode);
    }

    /// Seed the registry from an already-loaded config + best-effort
    /// connect-all (the runtime does this at startup so the TUI can later
    /// see what's there).
    pub async fn seed(&self, mcp: &BTreeMap<String, McpServerConfig>) {
        let mut guard = self.inner.lock().await;
        for (name, cfg) in mcp {
            let mut entry = Entry {
                config: cfg.clone(),
                server: None,
                tool_names: Vec::new(),
            };
            match wingman_mcp::connect(name, cfg).await {
                Ok(server) => {
                    self.register_server_tools(&server, &mut entry.tool_names);
                    entry.server = Some(server);
                }
                Err(e) => {
                    tracing::warn!("mcp seed: {name} failed to connect: {e}");
                }
            }
            guard.insert(name.clone(), entry);
        }
    }

    pub async fn list(&self) -> Vec<McpServerView> {
        let guard = self.inner.lock().await;
        guard
            .iter()
            .map(|(name, e)| McpServerView {
                name: name.clone(),
                command: e.config.command.clone(),
                args: e.config.args.clone(),
                connected: e.server.is_some(),
                tool_names: e.tool_names.clone(),
            })
            .collect()
    }

    /// Add a new server: write to config, connect, register tools.
    pub async fn add(&self, name: String, config: McpServerConfig) -> Result<(), String> {
        {
            let guard = self.inner.lock().await;
            if guard.contains_key(&name) {
                return Err(format!("server '{name}' already exists"));
            }
        }
        self.persist_set(&name, Some(&config)).await?;

        let mut entry = Entry {
            config: config.clone(),
            server: None,
            tool_names: Vec::new(),
        };
        match wingman_mcp::connect(&name, &config).await {
            Ok(server) => {
                self.register_server_tools(&server, &mut entry.tool_names);
                entry.server = Some(server);
            }
            Err(e) => return Err(format!("connect '{name}': {e}")),
        }
        self.inner.lock().await.insert(name, entry);
        Ok(())
    }

    /// Remove a server: deregister its tools, drop the connection, erase
    /// from the config file.
    pub async fn remove(&self, name: &str) -> Result<(), String> {
        let entry = {
            let mut guard = self.inner.lock().await;
            guard.remove(name)
        };
        let Some(entry) = entry else {
            return Err(format!("no server named '{name}'"));
        };
        for t in &entry.tool_names {
            self.tools.unregister(t);
        }
        drop(entry.server); // close stdio process
        self.persist_set(name, None).await?;
        Ok(())
    }

    /// Disconnect without forgetting the config entry.
    pub async fn disconnect(&self, name: &str) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        let entry = guard
            .get_mut(name)
            .ok_or_else(|| format!("no server '{name}'"))?;
        for t in entry.tool_names.drain(..) {
            self.tools.unregister(&t);
        }
        entry.server = None;
        Ok(())
    }

    /// (Re-)connect a server that we know about.
    pub async fn connect(&self, name: &str) -> Result<(), String> {
        // Pull the config out under the lock; release before we await
        // connect() so other registry methods don't block.
        let config = {
            let guard = self.inner.lock().await;
            guard
                .get(name)
                .ok_or_else(|| format!("no server '{name}'"))?
                .config
                .clone()
        };
        let server = wingman_mcp::connect(name, &config)
            .await
            .map_err(|e| format!("connect '{name}': {e}"))?;
        let mut guard = self.inner.lock().await;
        let entry = guard
            .get_mut(name)
            .ok_or_else(|| format!("no server '{name}'"))?;
        // If something connected in the meantime, deregister its tools
        // before swapping in the new connection.
        for t in entry.tool_names.drain(..) {
            self.tools.unregister(&t);
        }
        self.register_server_tools(&server, &mut entry.tool_names);
        entry.server = Some(server);
        Ok(())
    }

    /// Register every tool from `server` into the shared `ToolRegistry`,
    /// recording the names in `into` so we can deregister later.
    fn register_server_tools(&self, server: &McpServer, into: &mut Vec<String>) {
        for tool in &server.tools {
            let handle: Arc<dyn McpToolHandle> = Arc::new(McpTool::build(server, tool));
            let adapter: Arc<dyn wingman_tools::Tool> =
                Arc::new(McpToolAdapter::new(handle, server.trusted));
            let name = adapter.spec().name;
            self.tools.register_arc(adapter);
            into.push(name);
        }
    }

    /// Mutate the global config file's `[mcp.<name>]` section. `None`
    /// removes it. Reads → mutates → atomic-rewrites — same convention as
    /// [`Config::set_default_provider_and_save`].
    async fn persist_set(&self, name: &str, cfg: Option<&McpServerConfig>) -> Result<(), String> {
        let path = self.config_path.clone();
        if path.as_os_str().is_empty() {
            return Err("no global config path".into());
        }
        let name = name.to_string();
        let cfg = cfg.cloned();
        // Config I/O is sync; do it on the blocking pool so we don't
        // stall the runtime.
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let mut config = if path.exists() {
                Config::load(Some(&path), None).map_err(|e| format!("{e}"))?
            } else {
                Config::default()
            };
            match cfg {
                Some(c) => {
                    config.mcp.insert(name, c);
                }
                None => {
                    config.mcp.remove(&name);
                }
            }
            config.save_atomic(&path).map_err(|e| format!("{e}"))
        })
        .await
        .map_err(|e| format!("join: {e}"))?
    }
}
