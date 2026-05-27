use std::collections::BTreeMap;

use arccode_core::Usage;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

/// Tracks per-`provider/model` usage. The status line renders the rolled-up
/// total; the `/usage` modal renders a breakdown.
#[derive(Debug, Default, Clone)]
pub struct StatusLine {
    pub model: String,
    pub provider: String,
    pub mode: String,
    /// Key is `provider/model`. Empty when the user hasn't sent anything.
    pub usage: BTreeMap<String, Usage>,
    /// Whether the agent is currently connected / active.
    pub connected: bool,
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
        let dot_color = if s.connected { Color::Green } else { Color::Red };
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
        if !s.usage.is_empty() {
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
                    if let Some(price) = arccode_core::price_for(key) {
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
                spans.push(Span::styled(
                    "local",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::Reset))
            .render(area, buf);
    }
}
