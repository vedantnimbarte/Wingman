//! `/model` picker — flat fuzzy list of `provider/model` entries.
//!
//! The picker is restricted to the providers the user is actually connected
//! to (see `ModelPicker::new`); for each we expose a small curated set of
//! well-known models. The list is deliberately short; users who want an
//! obscure model can still type `/model provider/<model-id>` directly.

use crossterm::event::{KeyCode, KeyEvent};
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
};

use super::{centered_rect, ModalOutcome};

const VISIBLE_ROWS: usize = 12;

/// (provider_id, model_id) — what the picker emits on Enter.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug)]
pub struct ModelPicker {
    /// All known `provider/model` entries.
    entries: Vec<ModelChoice>,
    /// Indices into `entries` ranked by the current query.
    ranked: Vec<usize>,
    query: String,
    selected: usize,
    /// Connected providers whose live catalogs still need fetching. Drained
    /// once by the host loop via [`take_pending_task`](Self::take_pending_task).
    pending_providers: Option<Vec<String>>,
    /// True while the live-catalog fetch is in flight; drives the spinner
    /// hint in `render`.
    loading: bool,
}

impl ModelPicker {
    /// Build a picker limited to `connected` provider ids (the providers the
    /// user has logged into / configured). When `connected` is empty — e.g.
    /// no provider is set up yet — we fall back to the full catalog so the
    /// picker is never empty.
    pub fn new(connected: &[String]) -> Self {
        let entries = catalog(connected);
        let ranked: Vec<usize> = (0..entries.len()).collect();
        // Only fetch live catalogs when we have a bounded set of connected
        // providers; with no filter we'd hit every provider's API at once.
        let pending_providers = if connected.is_empty() {
            None
        } else {
            Some(connected.to_vec())
        };
        let loading = pending_providers.is_some();
        Self {
            entries,
            ranked,
            query: String::new(),
            selected: 0,
            pending_providers,
            loading,
        }
    }

    /// Returns the currently-highlighted choice, if any.
    pub fn take_selected(&mut self) -> Option<ModelChoice> {
        self.ranked
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
            .cloned()
    }

    /// Non-draining peek: is a live-catalog fetch still queued?
    pub fn has_pending_task(&self) -> bool {
        self.pending_providers.is_some()
    }

    /// Drain the one-shot list of providers whose live catalogs the host
    /// should fetch. Returns `Some` at most once per picker.
    pub fn take_pending_task(&mut self) -> Option<Vec<String>> {
        self.pending_providers.take()
    }

    /// Merge fetched model lists into the catalog. For every provider that
    /// returned at least one model we replace its static entries with the
    /// live list; providers absent from `fetched` — an unsupported endpoint
    /// or a failed request — keep their curated static entries. Clears the
    /// loading flag regardless.
    pub fn set_dynamic(&mut self, fetched: Vec<(String, Vec<String>)>) {
        self.loading = false;
        for (provider_id, models) in fetched {
            if models.is_empty() {
                continue;
            }
            self.entries.retain(|e| e.provider_id != provider_id);
            for model in models {
                self.entries.push(ModelChoice {
                    provider_id: provider_id.clone(),
                    model,
                });
            }
        }
        self.rerank();
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        match k.code {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.rerank();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.rerank();
            }
            KeyCode::Up if self.selected > 0 => {
                self.selected -= 1;
            }
            KeyCode::Down if self.selected + 1 < self.ranked.len() => {
                self.selected += 1;
            }
            KeyCode::Enter => {
                if self.ranked.is_empty() {
                    return ModalOutcome::Continue;
                }
                return ModalOutcome::Close;
            }
            _ => {}
        }
        ModalOutcome::Continue
    }

    fn rerank(&mut self) {
        self.selected = 0;
        if self.query.is_empty() {
            self.ranked = (0..self.entries.len()).collect();
            return;
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                buf.clear();
                let label = format!("{}/{}", e.provider_id, e.model);
                let needle = Utf32Str::new(&label, &mut buf);
                pattern.score(needle, &mut matcher).map(|s| (i, s))
            })
            .collect();
        scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        self.ranked = scored.into_iter().map(|(i, _)| i).collect();
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let rect = centered_rect(area, 60, 60);
        Clear.render(rect, buf);
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " /model — pick provider + model ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = outer.inner(rect);
        outer.render(rect, buf);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // filter
                Constraint::Min(3),    // list
                Constraint::Length(2), // hint
            ])
            .split(inner);

        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(self.query.clone(), Style::default().fg(Color::White)),
            Span::raw("▏"),
        ]))
        .render(chunks[0], buf);

        let height = chunks[1].height as usize;
        let visible = height.clamp(1, VISIBLE_ROWS);
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        let end = (start + visible).min(self.ranked.len());
        let items: Vec<ListItem> = self.ranked[start..end]
            .iter()
            .enumerate()
            .map(|(off, &idx)| {
                let i = start + off;
                let entry = &self.entries[idx];
                let marker = if i == self.selected { "› " } else { "  " };
                let line = Line::from(vec![
                    Span::styled(
                        format!("{marker}{:<12}", entry.provider_id),
                        if i == self.selected {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Cyan)
                        },
                    ),
                    Span::styled(
                        entry.model.clone(),
                        if i == self.selected {
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        },
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();
        List::new(items).render(chunks[1], buf);

        let mut hint = vec![Span::styled(
            "↑/↓ navigate · Enter switch · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )];
        if self.loading {
            hint.push(Span::styled(
                "  · fetching live models…",
                Style::default().fg(Color::Yellow),
            ));
        }
        Paragraph::new(Line::from(hint)).render(chunks[2], buf);
    }
}

impl Default for ModelPicker {
    fn default() -> Self {
        Self::new(&[])
    }
}

/// Curated catalog, filtered to the `connected` provider ids. Order roughly
/// reflects "first try this" preferences. An empty `connected` slice means
/// "no filter" — every provider is shown.
fn catalog(connected: &[String]) -> Vec<ModelChoice> {
    let entries: &[(&str, &[&str])] = &[
        (
            "anthropic",
            &[
                "claude-opus-4-7",
                "claude-sonnet-4-6",
                "claude-haiku-4-5-20251001",
            ],
        ),
        ("openai", &["gpt-4.1", "gpt-4o", "gpt-4o-mini", "o4-mini"]),
        (
            "openrouter",
            &[
                "anthropic/claude-opus-4-7",
                "anthropic/claude-sonnet-4-6",
                "openai/gpt-4.1",
                "google/gemini-2.5-pro",
                "meta-llama/llama-3.1-70b-instruct",
            ],
        ),
        (
            "gemini",
            &["gemini-2.5-pro", "gemini-2.5-flash", "gemini-1.5-pro"],
        ),
        (
            "litellm",
            &[
                "anthropic/claude-opus-4-7",
                "openai/gpt-4.1",
                "google/gemini-2.5-pro",
            ],
        ),
        ("lmstudio", &["local-model"]),
        ("vllm", &["local-model"]),
        ("ollama", &["llama3.1:8b", "qwen2.5:7b", "deepseek-r1:8b"]),
        (
            "groq",
            &[
                "llama-3.3-70b-versatile",
                "llama-3.1-8b-instant",
                "mixtral-8x7b-32768",
                "qwen-2.5-coder-32b",
            ],
        ),
        (
            "together",
            &[
                "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo",
                "meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "fireworks",
            &[
                "accounts/fireworks/models/llama-v3p1-70b-instruct",
                "accounts/fireworks/models/llama-v3p1-405b-instruct",
                "accounts/fireworks/models/qwen2p5-coder-32b-instruct",
            ],
        ),
        (
            "deepinfra",
            &[
                "meta-llama/Meta-Llama-3.1-70B-Instruct",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        ("perplexity", &["sonar-pro", "sonar", "sonar-reasoning-pro"]),
        (
            "xai",
            &["grok-2-latest", "grok-2-vision-latest", "grok-beta"],
        ),
        ("deepseek", &["deepseek-chat", "deepseek-reasoner"]),
        (
            "mistral",
            &[
                "mistral-large-latest",
                "mistral-small-latest",
                "codestral-latest",
            ],
        ),
        (
            "cerebras",
            &["llama3.1-70b", "llama3.1-8b", "llama-3.3-70b"],
        ),
        (
            "sambanova",
            &[
                "Meta-Llama-3.1-70B-Instruct",
                "Meta-Llama-3.1-405B-Instruct",
                "Meta-Llama-3.1-8B-Instruct",
            ],
        ),
        ("azure", &["gpt-4o", "gpt-4o-mini", "gpt-4.1"]),
        ("github", &["gpt-4o", "gpt-4o-mini", "Phi-3.5-MoE-instruct"]),
        ("llamacpp", &["local-model"]),
        ("tgi", &["local-model"]),
        (
            "anyscale",
            &[
                "meta-llama/Meta-Llama-3.1-70B-Instruct",
                "meta-llama/Meta-Llama-3.1-8B-Instruct",
                "mistralai/Mixtral-8x22B-Instruct-v0.1",
            ],
        ),
        ("lepton", &["llama3-1-70b", "llama3-1-8b", "qwen2-72b"]),
        (
            "replicate",
            &[
                "meta/meta-llama-3.1-405b-instruct",
                "meta/meta-llama-3.1-70b-instruct",
            ],
        ),
        (
            "novita",
            &[
                "meta-llama/llama-3.1-70b-instruct",
                "meta-llama/llama-3.1-8b-instruct",
                "qwen/qwen-2.5-72b-instruct",
            ],
        ),
        (
            "hyperbolic",
            &[
                "meta-llama/Meta-Llama-3.1-70B-Instruct",
                "deepseek-ai/DeepSeek-V3",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "lambda",
            &[
                "llama3.1-70b-instruct-fp8",
                "llama3.1-405b-instruct-fp8",
                "llama3.1-8b-instruct",
            ],
        ),
        (
            "nebius",
            &[
                "meta-llama/Meta-Llama-3.1-70B-Instruct-fast",
                "meta-llama/Meta-Llama-3.1-405B-Instruct",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "hf",
            &[
                "meta-llama/Llama-3.1-70B-Instruct",
                "meta-llama/Llama-3.3-70B-Instruct",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "glhf",
            &[
                "hf:meta-llama/Llama-3.1-70B-Instruct",
                "hf:Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "featherless",
            &[
                "meta-llama/Meta-Llama-3.1-8B-Instruct",
                "Qwen/Qwen2.5-72B-Instruct",
            ],
        ),
        (
            "octoai",
            &["meta-llama-3.1-70b-instruct", "meta-llama-3.1-8b-instruct"],
        ),
        (
            "nvidia",
            &[
                "meta/llama-3.1-70b-instruct",
                "meta/llama-3.1-405b-instruct",
                "nvidia/llama-3.1-nemotron-70b-instruct",
                "deepseek-ai/deepseek-r1",
            ],
        ),
        (
            "avian",
            &[
                "Meta-Llama-3.1-405B-Instruct",
                "Meta-Llama-3.1-70B-Instruct",
            ],
        ),
        (
            "kluster",
            &[
                "klusterai/Meta-Llama-3.1-405B-Instruct-Turbo",
                "klusterai/Meta-Llama-3.1-70B-Instruct-Turbo",
            ],
        ),
        (
            "inferencenet",
            &[
                "meta-llama/llama-3.1-70b-instruct",
                "meta-llama/llama-3.1-8b-instruct",
            ],
        ),
        (
            "snowflake",
            &["llama3.1-70b", "llama3.1-405b", "mistral-large2"],
        ),
        (
            "databricks",
            &[
                "databricks-meta-llama-3-1-70b-instruct",
                "databricks-meta-llama-3-1-405b-instruct",
                "databricks-dbrx-instruct",
            ],
        ),
        ("writer", &["palmyra-x5", "palmyra-x4", "palmyra-creative"]),
        (
            "cohere",
            &[
                "command-r-plus",
                "command-r",
                "command-a-03-2025",
                "command-light",
            ],
        ),
        (
            "qwen",
            &[
                "qwen-max",
                "qwen-plus",
                "qwen-turbo",
                "qwen2.5-coder-32b-instruct",
                "qwen-vl-max",
            ],
        ),
        (
            "zhipu",
            &["glm-4-plus", "glm-4-flash", "glm-4v-plus", "codegeex-4"],
        ),
        (
            "moonshot",
            &["moonshot-v1-128k", "moonshot-v1-32k", "moonshot-v1-8k"],
        ),
        (
            "minimax",
            &["abab6.5s-chat", "abab6.5-chat", "MiniMax-Text-01"],
        ),
        ("yi", &["yi-large", "yi-lightning", "yi-medium-200k"]),
        (
            "baichuan",
            &["Baichuan4-Turbo", "Baichuan4", "Baichuan3-Turbo"],
        ),
        (
            "hunyuan",
            &["hunyuan-pro", "hunyuan-standard", "hunyuan-turbo"],
        ),
        (
            "doubao",
            &[
                "doubao-pro-32k",
                "doubao-pro-128k",
                "doubao-1-5-pro-32k",
                "doubao-1-5-vision-pro-32k",
            ],
        ),
        (
            "siliconflow",
            &[
                "Qwen/Qwen2.5-72B-Instruct",
                "deepseek-ai/DeepSeek-V3",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "cloudflare",
            &[
                "@cf/meta/llama-3.1-70b-instruct",
                "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
                "@cf/qwen/qwen2.5-coder-32b-instruct",
            ],
        ),
        (
            "vercel",
            &["openai/gpt-4o", "anthropic/claude-3.5-sonnet", "xai/grok-2"],
        ),
        (
            "aimlapi",
            &[
                "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "deepseek-ai/DeepSeek-V3",
                "Qwen/Qwen2.5-Coder-32B-Instruct",
            ],
        ),
        (
            "openpipe",
            &["openpipe:meta-llama-3.1-70b", "openpipe:mistral-7b"],
        ),
        (
            "targon",
            &[
                "NousResearch/Hermes-3-Llama-3.1-70B",
                "meta-llama/Meta-Llama-3.1-70B-Instruct",
            ],
        ),
        ("pollinations", &["openai", "mistral", "qwen-coder"]),
        ("mlx", &["local-model"]),
        ("localai", &["local-model"]),
        ("aphrodite", &["local-model"]),
        ("mistralrs", &["local-model"]),
        (
            "ai21",
            &["jamba-1.5-large", "jamba-1.5-mini", "jamba-instruct"],
        ),
        ("zai", &["glm-4-plus", "glm-4-flash", "codegeex-4"]),
        (
            "friendli",
            &[
                "meta-llama-3.1-70b-instruct",
                "meta-llama-3.1-8b-instruct",
                "mixtral-8x7b-instruct-v0-1",
            ],
        ),
        ("mancer", &["weaver", "weaver-alpha", "mythomax-l2-13b"]),
        ("reka", &["reka-core", "reka-flash", "reka-edge"]),
        (
            "bedrock",
            &[
                "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
                "us.anthropic.claude-3-5-haiku-20241022-v1:0",
                "us.meta.llama3-3-70b-instruct-v1:0",
                "us.amazon.nova-pro-v1:0",
                "mistral.mistral-large-2407-v1:0",
            ],
        ),
        (
            "vertex",
            &[
                "google/gemini-1.5-pro-002",
                "google/gemini-1.5-flash-002",
                "google/gemini-2.0-flash-exp",
                "anthropic/claude-3-5-sonnet-v2@20241022",
                "meta/llama-3.1-70b-instruct-maas",
            ],
        ),
        (
            "watsonx",
            &[
                "ibm/granite-3-8b-instruct",
                "ibm/granite-3-2b-instruct",
                "meta-llama/llama-3-3-70b-instruct",
                "mistralai/mistral-large",
            ],
        ),
        ("gpt4all", &["local-model"]),
        ("jan", &["local-model"]),
        ("koboldcpp", &["local-model"]),
        ("oobabooga", &["local-model"]),
    ];
    let mut out = Vec::new();
    for (provider, models) in entries {
        if !connected.is_empty() && !connected.iter().any(|c| c == provider) {
            continue;
        }
        for m in *models {
            out.push(ModelChoice {
                provider_id: (*provider).into(),
                model: (*m).into(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_connected_shows_full_catalog() {
        // No filter -> the whole hardcoded catalog (many providers).
        assert!(catalog(&[]).len() > 100);
    }

    #[test]
    fn filters_to_connected_providers_only() {
        let connected = vec!["anthropic".to_string(), "openai".to_string()];
        let entries = catalog(&connected);
        assert!(!entries.is_empty());
        assert!(entries
            .iter()
            .all(|e| e.provider_id == "anthropic" || e.provider_id == "openai"));
        // Both connected providers actually contribute models.
        assert!(entries.iter().any(|e| e.provider_id == "anthropic"));
        assert!(entries.iter().any(|e| e.provider_id == "openai"));
    }

    #[test]
    fn unknown_connected_provider_yields_no_entries() {
        assert!(catalog(&["not-a-real-provider".to_string()]).is_empty());
    }

    #[test]
    fn connected_picker_queues_a_fetch_once() {
        let mut p = ModelPicker::new(&["openrouter".to_string()]);
        assert!(p.has_pending_task());
        assert_eq!(p.take_pending_task(), Some(vec!["openrouter".to_string()]));
        // Drains exactly once.
        assert!(!p.has_pending_task());
        assert_eq!(p.take_pending_task(), None);
    }

    #[test]
    fn empty_picker_queues_no_fetch() {
        let mut p = ModelPicker::new(&[]);
        assert!(!p.has_pending_task());
        assert_eq!(p.take_pending_task(), None);
    }

    #[test]
    fn set_dynamic_replaces_provider_entries() {
        let mut p = ModelPicker::new(&["openrouter".to_string()]);
        let static_count = p.entries.len();
        assert!(static_count > 0);
        p.set_dynamic(vec![(
            "openrouter".to_string(),
            vec!["a/model-1".to_string(), "b/model-2".to_string()],
        )]);
        // Static openrouter entries are gone; the two fetched ones remain.
        assert_eq!(p.entries.len(), 2);
        assert!(p.entries.iter().all(|e| e.provider_id == "openrouter"));
        assert!(p.entries.iter().any(|e| e.model == "a/model-1"));
        assert!(!p.loading);
    }

    #[test]
    fn set_dynamic_keeps_static_entries_for_empty_fetch() {
        let mut p = ModelPicker::new(&["openrouter".to_string()]);
        let static_count = p.entries.len();
        // An empty list (unsupported endpoint / failure) leaves statics intact.
        p.set_dynamic(vec![("openrouter".to_string(), vec![])]);
        assert_eq!(p.entries.len(), static_count);
        assert!(!p.loading);
    }
}
