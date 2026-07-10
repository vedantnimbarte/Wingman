//! wingman terminal UI built on ratatui + crossterm.
//!
//! Entry point: [`run`] takes an `AgentLoop` (already wired with provider,
//! tools, and config) and drives the interactive REPL until the user quits.

mod app;
mod attachments;
pub mod modal;
pub mod theme;
pub mod usage_store;
mod widgets;

pub use app::{
    run, AgentBuilder, AppCtx, LoginRunner, LogoutRunner, McpListRunner, McpRunner, ModeSetter,
    ModelsRunner, ProviderBuilder,
};
pub use theme::{init as init_theme, Theme};
