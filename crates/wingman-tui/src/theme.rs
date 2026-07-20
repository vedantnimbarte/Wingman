//! Resolved theme colors used by the transcript / status widgets.
//!
//! Built from the merged [`wingman_config::TuiConfig`]: named base theme
//! (`default` / `light` / `mono`) plus optional per-role overrides under
//! `tui.colors`. Initialized once at startup via [`init`] and read via
//! [`current`].

use ratatui::style::Color;
use std::sync::OnceLock;
use wingman_config::{ThemeColors, TuiConfig};

#[derive(Debug, Clone)]
pub struct Theme {
    pub user_prompt: Color,
    pub assistant: Color,
    pub tool_name: Color,
    pub tool_summary: Color,
    pub tool_ok: Color,
    pub tool_err: Color,
    pub system: Color,
    pub error: Color,
    pub code_block: Color,
}

static CURRENT: OnceLock<Theme> = OnceLock::new();

pub fn current() -> Theme {
    CURRENT.get().cloned().unwrap_or_else(default_theme)
}

pub fn init(cfg: &TuiConfig) {
    let _ = CURRENT.set(resolve(cfg));
}

fn resolve(cfg: &TuiConfig) -> Theme {
    let base = match cfg.theme.as_str() {
        "light" => light_theme(),
        "mono" => mono_theme(),
        _ => default_theme(),
    };
    apply_overrides(base, &cfg.colors)
}

fn apply_overrides(mut t: Theme, o: &ThemeColors) -> Theme {
    if let Some(c) = o.user_prompt.as_deref().and_then(parse_color) {
        t.user_prompt = c;
    }
    if let Some(c) = o.assistant.as_deref().and_then(parse_color) {
        t.assistant = c;
    }
    if let Some(c) = o.tool_name.as_deref().and_then(parse_color) {
        t.tool_name = c;
    }
    if let Some(c) = o.tool_summary.as_deref().and_then(parse_color) {
        t.tool_summary = c;
    }
    if let Some(c) = o.tool_ok.as_deref().and_then(parse_color) {
        t.tool_ok = c;
    }
    if let Some(c) = o.tool_err.as_deref().and_then(parse_color) {
        t.tool_err = c;
    }
    if let Some(c) = o.system.as_deref().and_then(parse_color) {
        t.system = c;
    }
    if let Some(c) = o.error.as_deref().and_then(parse_color) {
        t.error = c;
    }
    if let Some(c) = o.code_block.as_deref().and_then(parse_color) {
        t.code_block = c;
    }
    t
}

fn default_theme() -> Theme {
    Theme {
        user_prompt: Color::Cyan,
        assistant: Color::Reset,
        tool_name: Color::Yellow,
        tool_summary: Color::DarkGray,
        tool_ok: Color::Green,
        tool_err: Color::Red,
        system: Color::DarkGray,
        error: Color::Red,
        code_block: Color::Yellow,
    }
}

fn light_theme() -> Theme {
    Theme {
        user_prompt: Color::Blue,
        assistant: Color::Black,
        tool_name: Color::Magenta,
        tool_summary: Color::Gray,
        tool_ok: Color::Green,
        tool_err: Color::Red,
        system: Color::Gray,
        error: Color::Red,
        code_block: Color::Magenta,
    }
}

fn mono_theme() -> Theme {
    Theme {
        user_prompt: Color::White,
        assistant: Color::Reset,
        tool_name: Color::White,
        tool_summary: Color::DarkGray,
        tool_ok: Color::White,
        tool_err: Color::White,
        system: Color::DarkGray,
        error: Color::White,
        code_block: Color::DarkGray,
    }
}

fn parse_color(s: &str) -> Option<Color> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    Some(match t.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "gray" | "grey" => Color::Gray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "reset" | "default" => Color::Reset,
        _ => return None,
    })
}
