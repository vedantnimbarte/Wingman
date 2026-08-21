//! Terminal chrome shared by the pilot TUIs (`pilot watch`, `board`).
//!
//! Moved out of `pilot_watch_tui.rs` when the board grew a second full-screen
//! surface. Everything here is a **pure move** — same behaviour, same glyphs,
//! same raw-mode handling — so `pilot watch` renders exactly as it did before.
//!
//! Only what has two callers lives here. The log pane's `LogView`/`SevFilter`
//! and the watch-specific overlays stay in `pilot_watch_tui.rs` until
//! something else actually needs them.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// The glyph set the UI draws with. Unicode by default; the ASCII variant is
/// a portable fallback for terminals (legacy Windows console, non-UTF-8
/// locales) that render the fancier glyphs as tofu boxes.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    pub ascii: bool,
}

impl Glyphs {
    /// One glyph or the other, chosen by mode. Keeps the call sites readable.
    pub fn pick(&self, unicode: char, ascii: char) -> char {
        if self.ascii {
            ascii
        } else {
            unicode
        }
    }

    /// Animated progress spinner frame. Unicode rotates a quarter-filled
    /// disc; ASCII falls back to the classic `|/-\` spinner.
    pub fn spinner(&self, frame: u64) -> char {
        const UNI: [char; 4] = ['◐', '◓', '◑', '◒'];
        const ASC: [char; 4] = ['|', '/', '-', '\\'];
        let set = if self.ascii { &ASC } else { &UNI };
        set[(frame as usize) % set.len()]
    }

    pub fn current(&self) -> char {
        self.pick('▸', '>')
    }
    pub fn tool(&self) -> char {
        self.pick('▸', '>')
    }
    pub fn writes(&self) -> char {
        self.pick('✎', 'w')
    }
    pub fn running(&self) -> char {
        self.pick('▶', '>')
    }
    pub fn failed(&self) -> char {
        self.pick('✗', 'x')
    }
    pub fn blocked(&self) -> char {
        self.pick('‼', '!')
    }

    // ---- board-only glyphs -------------------------------------------------

    /// Collapsed / expanded card marker.
    pub fn collapsed(&self) -> char {
        self.pick('▸', '>')
    }
    pub fn expanded(&self) -> char {
        self.pick('▾', 'v')
    }
    pub fn done(&self) -> char {
        self.pick('✓', 'o')
    }
    /// Tree branch for a card's sub-rows.
    pub fn branch(&self) -> char {
        self.pick('├', '|')
    }
    pub fn branch_last(&self) -> char {
        self.pick('└', '`')
    }
}

pub fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

pub fn teardown(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// A centred box `pct_x` × `pct_y` percent of `area`, for modal overlays.
pub fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let w = area.width * pct_x / 100;
    let h = area.height * pct_y / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNI: Glyphs = Glyphs { ascii: false };
    const ASC: Glyphs = Glyphs { ascii: true };

    #[test]
    fn ascii_mode_swaps_every_glyph() {
        assert_eq!(UNI.failed(), '✗');
        assert_eq!(ASC.failed(), 'x');
        assert_eq!(UNI.expanded(), '▾');
        assert_eq!(ASC.expanded(), 'v');
    }

    #[test]
    fn spinner_cycles_without_panicking() {
        for f in 0..10u64 {
            assert!(!UNI.spinner(f).is_whitespace());
            assert!(!ASC.spinner(f).is_whitespace());
        }
    }

    #[test]
    fn centered_fits_inside_its_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let c = centered(60, 50, area);
        assert!(c.x + c.width <= area.width);
        assert!(c.y + c.height <= area.height);
        // Degenerate terminals must not produce an out-of-bounds rect.
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let c = centered(80, 80, tiny);
        assert!(c.width <= 1 && c.height <= 1);
    }
}
