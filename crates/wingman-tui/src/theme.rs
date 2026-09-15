//! Resolved theme colors used by the transcript / status widgets.
//!
//! Built from the merged [`wingman_config::TuiConfig`]: named base theme
//! (`default` / `light` / `mono`) plus optional per-role overrides under
//! `tui.colors`. Initialized once at startup via [`init`] and read via
//! [`current`].
//!
//! A non-empty `NO_COLOR` (<https://no-color.org>) wins over both: code falls
//! back to the `mono` syntax styles, and [`strip_colour`] clears every colour
//! from each drawn frame, including the ones widgets pick for themselves.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
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
    /// How code is told apart by syntax scope.
    pub syntax: Syntax,
    /// `NO_COLOR` is set: frames are drawn without colour.
    pub no_color: bool,
}

/// Palette for syntax-highlighted code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// Hues chosen to read on a dark ground.
    Dark,
    /// Darker hues for a light ground.
    Light,
    /// No hue: scopes differ only by weight and slant.
    Mono,
}

static CURRENT: OnceLock<Theme> = OnceLock::new();

pub fn current() -> Theme {
    CURRENT.get().cloned().unwrap_or_else(default_theme)
}

pub fn init(cfg: &TuiConfig) {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let _ = CURRENT.set(resolve(cfg, no_color));
}

pub(crate) fn resolve(cfg: &TuiConfig, no_color: bool) -> Theme {
    if no_color {
        return Theme {
            no_color: true,
            ..mono_theme()
        };
    }
    let base = match cfg.theme.as_str() {
        "light" => light_theme(),
        "mono" => mono_theme(),
        _ => default_theme(),
    };
    apply_overrides(base, &cfg.colors)
}

/// Clear every foreground and background colour in `buf`. A cell that had a
/// background (a selected row, a button) is reversed instead, so what the
/// colour singled out stays visible without it.
pub fn strip_colour(buf: &mut Buffer) {
    for cell in &mut buf.content {
        if cell.bg != Color::Reset {
            cell.modifier.insert(Modifier::REVERSED);
        }
        cell.set_fg(Color::Reset).set_bg(Color::Reset);
    }
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
        syntax: Syntax::Dark,
        no_color: false,
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
        syntax: Syntax::Light,
        no_color: false,
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
        syntax: Syntax::Mono,
        no_color: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, style::Style};

    #[test]
    fn no_color_ignores_overrides_and_strips_frames() {
        let cfg = TuiConfig {
            colors: ThemeColors {
                code_block: Some("red".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve(&cfg, false).code_block, Color::Red);
        let th = resolve(&cfg, true);
        assert!(th.no_color);
        assert_eq!(th.syntax, Syntax::Mono);

        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf.set_string(0, 0, "a", Style::default().fg(Color::Green));
        buf.set_string(1, 0, "b", Style::default().fg(Color::Black).bg(Color::Red));
        strip_colour(&mut buf);
        let (a, b) = (&buf.content[0], &buf.content[1]);
        assert_eq!(
            (a.fg, a.bg, b.fg, b.bg),
            (Color::Reset, Color::Reset, Color::Reset, Color::Reset)
        );
        assert!(!a.modifier.contains(Modifier::REVERSED));
        assert!(b.modifier.contains(Modifier::REVERSED));
    }
}
