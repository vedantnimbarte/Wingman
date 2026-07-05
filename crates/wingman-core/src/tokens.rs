//! Token-saving pipeline.
//!
//! M2 ships [`ToolOutputBudget`] and [`Compactor`]. The [`CacheStrategy`]
//! abstraction is per-provider and lives at each adapter (Anthropic places
//! `cache_control` markers; OpenAI relies on stable prefix ordering; Gemini
//! uses `cachedContent` resources, plumbed in a follow-up).

use crate::{ContentBlock, Message, Role};

/// Cap tool output size before it's fed back to the model. The full output
/// stays in the session log; what the model sees is head/tail with an
/// elision marker in the middle.
#[derive(Debug, Clone, Copy)]
pub struct ToolOutputBudget {
    /// Maximum number of lines fed to the model from a single tool result.
    pub max_lines: u32,
}

impl Default for ToolOutputBudget {
    fn default() -> Self {
        Self { max_lines: 400 }
    }
}

impl ToolOutputBudget {
    pub fn new(max_lines: u32) -> Self {
        Self { max_lines }
    }

    /// Returns the trimmed body. If the input fits, returns it unchanged.
    pub fn trim(&self, body: &str) -> String {
        if self.max_lines == 0 {
            return body.to_string();
        }
        let lines: Vec<&str> = body.lines().collect();
        let total = lines.len();
        let budget = self.max_lines as usize;
        if total <= budget {
            return body.to_string();
        }
        let head = budget / 2;
        let tail = budget - head;
        let elided = total - head - tail;
        let mut out = String::with_capacity(body.len());
        for line in &lines[..head] {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!(
            "… {elided} lines elided (full output in session log) …\n"
        ));
        for line in &lines[total - tail..] {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Crude character-based token estimator. We don't ship a real tokenizer
/// in M2 — the rough heuristic (~4 chars/token) is good enough to decide
/// *when* to compact; the provider returns authoritative counts.
pub fn estimate_tokens(s: &str) -> u32 {
    let chars = s.chars().count();
    chars.div_ceil(4) as u32
}

/// Estimate the token cost of a full message history. Includes a small
/// per-message overhead because providers add envelope tokens around every
/// message.
pub fn estimate_history_tokens(history: &[Message], system: Option<&str>) -> u32 {
    let mut total: u32 = 4; // request envelope
    if let Some(s) = system {
        total = total.saturating_add(estimate_tokens(s) + 4);
    }
    for m in history {
        total = total.saturating_add(8); // per-message overhead
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => {
                    total = total.saturating_add(estimate_tokens(text));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    total = total
                        .saturating_add(estimate_tokens(name))
                        .saturating_add(estimate_tokens(&input.to_string()));
                }
                ContentBlock::ToolResult { content, .. } => {
                    total = total.saturating_add(estimate_tokens(content));
                }
                // Image data is large binary — use a conservative fixed estimate.
                ContentBlock::Image { data, .. } => {
                    // base64 data length / 4 chars-per-token (same heuristic as text)
                    total = total.saturating_add(estimate_tokens(data));
                }
            }
        }
    }
    total
}

/// Compaction policy. When estimated context > `trigger_tokens`, the
/// agent loop summarizes the oldest non-recap span into a single
/// recap message and rewrites history.
#[derive(Debug, Clone, Copy)]
pub struct Compactor {
    /// Trigger threshold. Compaction runs when `estimate_history_tokens`
    /// crosses this value before a request is sent.
    pub trigger_tokens: u32,
    /// Always keep the most recent N messages intact.
    pub keep_recent: usize,
}

impl Default for Compactor {
    fn default() -> Self {
        Self {
            trigger_tokens: 120_000,
            keep_recent: 6,
        }
    }
}

/// Result of a compaction pass: a single user-role message that replaces
/// the compacted prefix, plus the count of messages that were folded.
#[derive(Debug, Clone)]
pub struct CompactPlan {
    pub recap: Message,
    pub replaced: usize,
}

impl Compactor {
    /// Returns a plan if compaction should run, or `None` if the history
    /// is under budget or too short to be worth folding.
    pub fn plan(&self, history: &[Message], system: Option<&str>) -> Option<CompactPlan> {
        if estimate_history_tokens(history, system) < self.trigger_tokens {
            return None;
        }
        self.plan_forced(history)
    }

    /// Build a compaction plan ignoring the token threshold — used by an
    /// on-demand `/compact`. Still returns `None` when there's nothing worth
    /// folding (history no longer than `keep_recent`).
    pub fn plan_forced(&self, history: &[Message]) -> Option<CompactPlan> {
        if history.len() <= self.keep_recent {
            return None;
        }
        let split = history.len() - self.keep_recent;
        let to_fold = &history[..split];

        let summary = synthesize_recap(to_fold);
        let recap = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "[wingman compact] earlier messages folded into recap:\n\n{summary}\n\n\
                     Continue from here as if the conversation above had occurred."
                ),
            }],
        };
        Some(CompactPlan {
            recap,
            replaced: split,
        })
    }
}

/// Plain-text summary of a span of messages. We don't call out to an LLM
/// from inside `wingman-core` (it has no Provider here) — instead we
/// produce a structured outline that captures roles, tool calls, and
/// outcomes. A future enhancement will route this through the fast model.
fn synthesize_recap(messages: &[Message]) -> String {
    let mut out = String::new();
    for (i, m) in messages.iter().enumerate() {
        let role = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
        };
        let mut summary = String::new();
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => {
                    let first = text.lines().next().unwrap_or("").trim();
                    if !first.is_empty() {
                        if !summary.is_empty() {
                            summary.push_str("; ");
                        }
                        summary.push_str(&truncate_chars(first, 200));
                    }
                }
                ContentBlock::ToolUse { name, .. } => {
                    if !summary.is_empty() {
                        summary.push_str("; ");
                    }
                    summary.push_str(&format!("called {name}"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let first = content.lines().next().unwrap_or("").trim();
                    if !summary.is_empty() {
                        summary.push_str("; ");
                    }
                    summary.push_str(&format!(
                        "tool {} → {}",
                        if *is_error { "errored" } else { "ok" },
                        truncate_chars(first, 100)
                    ));
                }
                ContentBlock::Image { media_type, .. } => {
                    if !summary.is_empty() {
                        summary.push_str("; ");
                    }
                    summary.push_str(&format!("image ({media_type})"));
                }
            }
        }
        if summary.is_empty() {
            continue;
        }
        out.push_str(&format!("{}. {role}: {summary}\n", i + 1));
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_passes_short_output_through() {
        let b = ToolOutputBudget::new(10);
        let s = "a\nb\nc\n";
        assert_eq!(b.trim(s).trim_end(), "a\nb\nc");
    }

    #[test]
    fn budget_truncates_long_output_with_elision_marker() {
        let b = ToolOutputBudget::new(4);
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let body = lines.join("\n");
        let out = b.trim(&body);
        assert!(out.contains("line 0"));
        assert!(out.contains("line 19"));
        assert!(out.contains("elided"));
        assert!(!out.contains("line 10"));
    }

    #[test]
    fn compactor_does_nothing_under_threshold() {
        let c = Compactor {
            trigger_tokens: 10_000,
            keep_recent: 2,
        };
        let history = vec![
            Message::user_text("hi"),
            Message::assistant(vec![ContentBlock::text("hello")]),
        ];
        assert!(c.plan(&history, None).is_none());
    }

    #[test]
    fn compactor_folds_old_messages_above_threshold() {
        let c = Compactor {
            trigger_tokens: 50,
            keep_recent: 1,
        };
        let big = "x".repeat(500);
        let history = vec![
            Message::user_text(big.clone()),
            Message::assistant(vec![ContentBlock::text("reply")]),
            Message::user_text("again"),
        ];
        let plan = c.plan(&history, None).expect("should compact");
        assert_eq!(plan.replaced, 2);
        if let ContentBlock::Text { text } = &plan.recap.content[0] {
            assert!(text.contains("recap"));
        } else {
            panic!("recap should be text");
        }
    }

    #[test]
    fn plan_forced_ignores_threshold() {
        let c = Compactor {
            trigger_tokens: 1_000_000, // unreachably high
            keep_recent: 2,
        };
        let history = vec![
            Message::user_text("one"),
            Message::assistant(vec![ContentBlock::text("two")]),
            Message::user_text("three"),
            Message::assistant(vec![ContentBlock::text("four")]),
        ];
        // Under threshold, the automatic path does nothing…
        assert!(c.plan(&history, None).is_none());
        // …but forced compaction folds all but keep_recent.
        assert_eq!(c.plan_forced(&history).unwrap().replaced, 2);
        // Too little to fold → None even when forced.
        assert!(c.plan_forced(&history[..2]).is_none());
    }

    #[test]
    fn estimator_counts_tool_call_args() {
        let m = Message::assistant(vec![ContentBlock::ToolUse {
            id: "x".into(),
            name: "edit_file".into(),
            input: json!({"path": "src/main.rs", "old_string": "foo", "new_string": "bar"}),
        }]);
        let n = estimate_history_tokens(&[m], None);
        assert!(n > 10);
    }
}
