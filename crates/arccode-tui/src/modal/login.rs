//! Provider login wizard — the `/login` modal.
//!
//! State machine:
//!     PickProvider → EnterKey / EnterBaseUrl → EnterModel → Testing
//!         → Committing → Done
//!     (any of Testing/Committing can land in Failed; Enter retries)
//!
//! Async work (probe, commit) is dispatched back to the host loop via
//! [`take_pending_task`] / [`task_completed`] — the modal itself stays sync.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

/// Stable id, display label, default model, whether it needs an API key,
/// and the default base URL for local providers.
pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "anthropic",
        label: "Anthropic (Claude)",
        default_model: "claude-opus-4-7",
        needs_key: true,
        default_base_url: None,
    },
    ProviderSpec {
        id: "openai",
        label: "OpenAI",
        default_model: "gpt-4.1",
        needs_key: true,
        default_base_url: None,
    },
    ProviderSpec {
        id: "openrouter",
        label: "OpenRouter",
        default_model: "anthropic/claude-opus-4-7",
        needs_key: true,
        default_base_url: None,
    },
    ProviderSpec {
        id: "gemini",
        label: "Google Gemini",
        default_model: "gemini-2.5-pro",
        needs_key: true,
        default_base_url: None,
    },
    ProviderSpec {
        id: "litellm",
        label: "LiteLLM proxy",
        default_model: "anthropic/claude-opus-4-7",
        needs_key: true,
        default_base_url: Some("http://localhost:4000/v1"),
    },
    ProviderSpec {
        id: "lmstudio",
        label: "LM Studio (local)",
        default_model: "local-model",
        needs_key: false,
        default_base_url: Some("http://localhost:1234/v1"),
    },
    ProviderSpec {
        id: "vllm",
        label: "vLLM (local)",
        default_model: "local-model",
        needs_key: false,
        default_base_url: Some("http://localhost:8000/v1"),
    },
    ProviderSpec {
        id: "ollama",
        label: "Ollama (local)",
        default_model: "llama3.1:8b",
        needs_key: false,
        default_base_url: Some("http://localhost:11434/v1"),
    },
];

#[derive(Debug, Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub default_model: &'static str,
    pub needs_key: bool,
    pub default_base_url: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    PickProvider,
    EnterKey,
    EnterBaseUrl,
    EnterModel,
    Testing,
    Committing,
    Done,
}

/// Snapshot of the wizard fields the host needs to perform Probe / Commit.
#[derive(Debug, Clone)]
pub struct LoginPayload {
    pub provider_id: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: String,
}

/// An async task the wizard is waiting on the host to perform.
#[derive(Debug, Clone)]
pub enum LoginTask {
    Probe(LoginPayload),
    Commit(LoginPayload),
}

#[derive(Debug)]
pub struct LoginWizard {
    stage: Stage,
    provider_idx: usize,
    api_key: String,
    base_url: String,
    model: String,
    /// Set when the most recent async task failed. Cleared on retry.
    error: Option<String>,
    /// Task the host should pick up and execute. Drained by
    /// [`take_pending_task`]; refilled when the stage transitions into
    /// Testing or Committing.
    pending: Option<LoginTask>,
}

impl LoginWizard {
    pub fn new() -> Self {
        let spec = PROVIDERS[0];
        Self {
            stage: Stage::PickProvider,
            provider_idx: 0,
            api_key: String::new(),
            base_url: spec.default_base_url.unwrap_or("").to_string(),
            model: spec.default_model.to_string(),
            error: None,
            pending: None,
        }
    }

    fn spec(&self) -> ProviderSpec {
        PROVIDERS[self.provider_idx]
    }

    fn select_provider(&mut self, idx: usize) {
        self.provider_idx = idx.min(PROVIDERS.len() - 1);
        let spec = self.spec();
        self.api_key.clear();
        self.base_url = spec.default_base_url.unwrap_or("").to_string();
        self.model = spec.default_model.to_string();
    }

    fn payload(&self) -> LoginPayload {
        let spec = self.spec();
        LoginPayload {
            provider_id: spec.id.to_string(),
            api_key: if spec.needs_key && !self.api_key.is_empty() {
                Some(self.api_key.clone())
            } else {
                None
            },
            base_url: if self.base_url.is_empty() {
                None
            } else {
                Some(self.base_url.clone())
            },
            model: self.model.clone(),
        }
    }

    /// Drain the pending async task. Host runs it then reports via
    /// [`task_completed`].
    pub fn take_pending_task(&mut self) -> Option<LoginTask> {
        self.pending.take()
    }

    /// Host reports completion of the most recently dispatched task.
    pub fn task_completed(&mut self, result: Result<(), String>) {
        match (self.stage, result) {
            (Stage::Testing, Ok(())) => {
                // Probe succeeded — proceed to commit.
                self.stage = Stage::Committing;
                self.pending = Some(LoginTask::Commit(self.payload()));
            }
            (Stage::Committing, Ok(())) => {
                self.stage = Stage::Done;
            }
            (_, Err(msg)) => {
                self.error = Some(msg);
                // Drop back to model entry so the user can edit and retry.
                self.stage = Stage::EnterModel;
            }
            _ => {}
        }
    }

    /// True once the wizard has finished and the host should close it and
    /// adopt the new provider.
    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Snapshot of the data the host should adopt on close.
    pub fn final_payload(&self) -> LoginPayload {
        self.payload()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        // Async stages eat keys; the host pump drives them.
        if matches!(self.stage, Stage::Testing | Stage::Committing | Stage::Done) {
            return ModalOutcome::Continue;
        }
        self.error = None;
        match self.stage {
            Stage::PickProvider => self.handle_pick_provider(k),
            Stage::EnterKey => self.handle_text(k, TextField::Key),
            Stage::EnterBaseUrl => self.handle_text(k, TextField::BaseUrl),
            Stage::EnterModel => self.handle_text(k, TextField::Model),
            _ => ModalOutcome::Continue,
        }
    }

    fn handle_pick_provider(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Up => {
                if self.provider_idx == 0 {
                    self.select_provider(PROVIDERS.len() - 1);
                } else {
                    self.select_provider(self.provider_idx - 1);
                }
            }
            KeyCode::Down => {
                self.select_provider((self.provider_idx + 1) % PROVIDERS.len());
            }
            KeyCode::Enter => {
                let spec = self.spec();
                self.stage = if spec.needs_key {
                    Stage::EnterKey
                } else {
                    Stage::EnterBaseUrl
                };
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    fn handle_text(&mut self, k: KeyEvent, field: TextField) -> ModalOutcome {
        let buf = match field {
            TextField::Key => &mut self.api_key,
            TextField::BaseUrl => &mut self.base_url,
            TextField::Model => &mut self.model,
        };
        match k.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => {
                let spec = self.spec();
                match field {
                    TextField::Key => {
                        if self.api_key.trim().is_empty() {
                            self.error = Some("API key required".into());
                            return ModalOutcome::Continue;
                        }
                        // Remote providers may also want a custom base_url
                        // (proxies etc.) — but for v1 only show it for the
                        // ones that ship with a default.
                        if spec.default_base_url.is_some() {
                            self.stage = Stage::EnterBaseUrl;
                        } else {
                            self.stage = Stage::EnterModel;
                        }
                    }
                    TextField::BaseUrl => {
                        self.stage = Stage::EnterModel;
                    }
                    TextField::Model => {
                        if self.model.trim().is_empty() {
                            self.error = Some("model required".into());
                            return ModalOutcome::Continue;
                        }
                        self.stage = Stage::Testing;
                        self.pending = Some(LoginTask::Probe(self.payload()));
                    }
                }
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 70, 70);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /login — connect a provider ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // step indicator
                Constraint::Min(3),    // body
                Constraint::Length(2), // hint / error
            ])
            .split(inner);

        self.render_step(chunks[0], buf);
        self.render_body(chunks[1], buf);
        self.render_hint(chunks[2], buf);
    }

    fn render_step(&self, area: Rect, buf: &mut Buffer) {
        let label = match self.stage {
            Stage::PickProvider => "Step 1/4 · pick a provider",
            Stage::EnterKey => "Step 2/4 · enter API key",
            Stage::EnterBaseUrl => "Step 2/4 · base URL",
            Stage::EnterModel => "Step 3/4 · pick model",
            Stage::Testing => "Step 4/4 · testing connection",
            Stage::Committing => "Step 4/4 · saving",
            Stage::Done => "Done",
        };
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default().fg(Color::DarkGray),
        )))
        .render(area, buf);
    }

    fn render_body(&self, area: Rect, buf: &mut Buffer) {
        match self.stage {
            Stage::PickProvider => {
                let items: Vec<ListItem> = PROVIDERS
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let marker = if i == self.provider_idx { "› " } else { "  " };
                        let style = if i == self.provider_idx {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{marker}{}", p.label),
                            style,
                        )))
                    })
                    .collect();
                List::new(items).render(area, buf);
            }
            Stage::EnterKey => {
                let masked: String = "*".repeat(self.api_key.chars().count());
                let line = Line::from(vec![
                    Span::styled("API key: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(masked, Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::EnterBaseUrl => {
                let line = Line::from(vec![
                    Span::styled("Base URL: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.base_url.clone(), Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::EnterModel => {
                let line = Line::from(vec![
                    Span::styled("Model: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(self.model.clone(), Style::default().fg(Color::White)),
                    Span::raw("▏"),
                ]);
                Paragraph::new(line).render(area, buf);
            }
            Stage::Testing => {
                Paragraph::new(Line::from(Span::styled(
                    "Testing connection…",
                    Style::default().fg(Color::Yellow),
                )))
                .render(area, buf);
            }
            Stage::Committing => {
                Paragraph::new(Line::from(Span::styled(
                    "Saving credentials and building agent…",
                    Style::default().fg(Color::Yellow),
                )))
                .render(area, buf);
            }
            Stage::Done => {
                Paragraph::new(Line::from(Span::styled(
                    "Connected.",
                    Style::default().fg(Color::Green),
                )))
                .render(area, buf);
            }
        }
    }

    fn render_hint(&self, area: Rect, buf: &mut Buffer) {
        let line = if let Some(err) = &self.error {
            Line::from(vec![
                Span::styled("error: ", Style::default().fg(Color::Red)),
                Span::styled(err.clone(), Style::default().fg(Color::Red)),
            ])
        } else {
            let hint = match self.stage {
                Stage::PickProvider => "↑/↓ navigate · Enter select · Esc cancel",
                Stage::EnterKey => "type key (hidden) · Enter continue · Esc cancel",
                Stage::EnterBaseUrl | Stage::EnterModel => {
                    "Enter continue · Backspace edit · Esc cancel"
                }
                Stage::Testing | Stage::Committing => "(working…)",
                Stage::Done => "press Esc to close",
            };
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        };
        Paragraph::new(line).render(area, buf);
    }
}

impl Default for LoginWizard {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum TextField {
    Key,
    BaseUrl,
    Model,
}
