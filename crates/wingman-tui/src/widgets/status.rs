use std::collections::BTreeMap;

use wingman_core::Usage;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Tracks per-`provider/model` usage. The status line renders the rolled-up
/// total; the `/usage` modal renders a breakdown.
#[derive(Debug, Clone)]
pub struct StatusLine {
    pub model: String,
    pub provider: String,
    pub mode: String,
    /// Key is `provider/model`. Empty when the user hasn't sent anything.
    pub usage: BTreeMap<String, Usage>,
    /// Whether the agent is currently connected / active.
    pub connected: bool,
    /// Current git branch, if the working dir is a repo. Refreshed on turn
    /// boundaries and after `/commit`.
    pub git_branch: Option<String>,
    /// Number of changed (tracked + untracked) files per `git status`.
    pub git_dirty: usize,
    /// Whether to show the `tok in:… out:… cache:…% $cost` segment. Wired from
    /// `[tui].show_token_usage`; the `/usage` modal is unaffected.
    pub show_token_usage: bool,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: String::new(),
            mode: String::new(),
            usage: BTreeMap::new(),
            connected: false,
            git_branch: None,
            git_dirty: 0,
            show_token_usage: true,
        }
    }
}

impl StatusLine {
    /// Refresh `git_branch`/`git_dirty` by shelling out to `git` in `root`.
    /// Cheap enough to call on turn boundaries; silently clears on non-repos.
    pub fn refresh_git(&mut self, root: &std::path::Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        };
        // `branch --show-current` is Some even on an unborn branch and empty
        // on detached HEAD (unlike `rev-parse --abbrev-ref`, which errors).
        self.git_branch = run(&["branch", "--show-current"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.git_dirty = run(&["status", "--porcelain"])
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
    }
}

impl StatusLine {
    /// Merge usage into the slot for the *currently active* provider+model.
    /// Called from `apply_event` when the agent stream emits Usage.
    pub fn merge_usage(&mut self, u: &Usage) {
        if self.provider.is_empty() || self.model.is_empty() {
            return;
        }
        let key = format!("{}/{}", self.provider, self.model);
        self.usage.entry(key).or_default().add(u);
    }

    /// Sum across every model used this session.
    pub fn total(&self) -> Usage {
        let mut total = Usage::default();
        for u in self.usage.values() {
            total.add(u);
        }
        total
    }
}

pub struct StatusView<'a> {
    pub status: &'a StatusLine,
}

impl<'a> Widget for StatusView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let s = self.status;
        let total = s.total();
        let cache_hit_pct = (total.cache_hit_ratio() * 100.0).round() as u32;
        let provider_label = if s.provider.is_empty() {
            "no provider".to_string()
        } else {
            s.provider.clone()
        };
        let dot_color = if s.connected {
            Color::Green
        } else {
            Color::Red
        };
        let mut spans = vec![
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(
                format!(" {provider_label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(s.model.clone(), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled(
                format!("mode={}", s.mode),
                Style::default().fg(Color::DarkGray),
            ),
        ];
        if let Some(branch) = &s.git_branch {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("⎇ {branch}"),
                Style::default().fg(Color::Magenta),
            ));
            if s.git_dirty > 0 {
                spans.push(Span::styled(
                    format!("*{}", s.git_dirty),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        if s.show_token_usage && !s.usage.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!(
                    "tok in:{} out:{} cache:{}%",
                    total.input_tokens + total.cache_creation_input_tokens,
                    total.output_tokens,
                    cache_hit_pct
                ),
                Style::default().fg(Color::DarkGray),
            ));

            let cost_usd = {
                let mut c = 0.0f64;
                for (key, u) in &s.usage {
                    if let Some(price) = wingman_core::price_for(key) {
                        c += price.cost(u);
                    }
                }
                c
            };

            spans.push(Span::raw("  "));
            if cost_usd > 0.0 {
                spans.push(Span::styled(
                    format!("${:.4}", cost_usd),
                    Style::default().fg(Color::Green),
                ));
            } else if !s.usage.is_empty() {
                // Provider with no pricing data (e.g. local model)
                spans.push(Span::styled("local", Style::default().fg(Color::DarkGray)));
            }
        }
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Reset))
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    fn rendered_text(s: &StatusLine) -> String {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        StatusView { status: s }.render(area, &mut buf);
        (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn show_token_usage_gates_the_token_segment() {
        let mut s = StatusLine {
            provider: "anthropic".into(),
            model: "claude".into(),
            ..Default::default()
        };
        s.merge_usage(&Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        });
        assert!(rendered_text(&s).contains("tok"), "shown by default");
        s.show_token_usage = false;
        assert!(!rendered_text(&s).contains("tok"), "hidden when disabled");
    }

    #[test]
    fn refresh_git_tracks_branch_and_dirty() {
        let dir = std::env::temp_dir().join(format!("wingman-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(
            &dir,
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"],
        );

        let mut s = StatusLine::default();
        s.refresh_git(&dir);
        assert!(s.git_branch.is_some());
        assert_eq!(s.git_dirty, 0);

        std::fs::write(dir.join("f.txt"), "x").unwrap();
        s.refresh_git(&dir);
        assert_eq!(s.git_dirty, 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
