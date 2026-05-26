//! arccode terminal UI built on ratatui + crossterm.
//!
//! Entry point: [`run`] takes an `AgentLoop` (already wired with provider,
//! tools, and config) and drives the interactive REPL until the user quits.

mod app;
mod widgets;

pub use app::{run, AppCtx, ProviderBuilder};
