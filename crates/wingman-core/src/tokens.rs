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

    /// Whether [`trim`](Self::trim) would drop anything from `body`.
    ///
    /// The caller cannot infer this by comparing lengths: eliding a handful of
    /// very short lines can make the trimmed form *longer* than the original
    /// once the marker is added, so a length comparison would report "nothing
    /// was lost" on a result that did lose a line. Spilling keys off this, and
    /// it has to agree with `trim` exactly — hence one predicate, used by both.
    pub fn would_trim(&self, body: &str) -> bool {
        self.max_lines != 0 && body.lines().count() > self.max_lines as usize
    }

    /// Returns the trimmed body. If the input fits, returns it unchanged.
    pub fn trim(&self, body: &str) -> String {
        if !self.would_trim(body) {
            return body.to_string();
        }
        let lines: Vec<&str> = body.lines().collect();
        let total = lines.len();
        let budget = self.max_lines as usize;
        let head = budget / 2;
        let tail = budget - head;
        let elided = total - head - tail;
        let mut out = String::with_capacity(body.len());
        for line in &lines[..head] {
            out.push_str(line);
            out.push('\n');
        }
        // No claim about where the rest went: when spilling is on, the
        // locator line at the head of the result says so precisely, and when
        // it is off the model has no way to reach the full text anyway.
        // Pointing it at the session log was an instruction it could not act
        // on.
        out.push_str(&format!("… {elided} lines elided …\n"));
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
                // Reasoning is re-sent verbatim on every subsequent turn, so
                // it occupies context exactly like text does and has to be
                // counted or compaction triggers late.
                ContentBlock::Thinking { text, .. } => {
                    total = total.saturating_add(estimate_tokens(text));
                }
            }
        }
    }
    total
}

/// Shrink oversized tool results in place, before compaction folds whole
/// turns away.
///
/// Compaction is the blunt instrument: it replaces a span of history with a
/// recap, which discards the assistant's reasoning along with the bulk. But
/// the bulk and the reasoning are not the same thing — in a long session the
/// tokens are overwhelmingly in tool *results*, while the value is in what
/// the assistant concluded from them. Pruning takes the first without
/// touching the second, which buys many more turns before anything has to be
/// folded at all.
///
/// Model-free on purpose: this runs on every over-budget turn, and paying for
/// a summarization call each time would defeat the point.
#[derive(Debug, Clone, Copy)]
pub struct ToolResultPruner {
    /// Prune a tool result whose content exceeds this many characters.
    pub threshold_chars: usize,
    /// Leading characters kept.
    pub head_chars: usize,
    /// Trailing characters kept.
    pub tail_chars: usize,
    /// Never prune within the last N messages. The results the model is
    /// actively working from must survive intact; pruning what it just asked
    /// for would make it ask again, which costs more than it saves.
    pub keep_recent: usize,
}

impl Default for ToolResultPruner {
    fn default() -> Self {
        Self {
            threshold_chars: 8192,
            head_chars: 4096,
            tail_chars: 1024,
            keep_recent: 6,
        }
    }
}

/// Marker left in place of the characters a prune removed.
const PRUNE_MARKER: &str =
    "\n… [wingman] middle of this earlier tool result pruned to save context …\n";

impl ToolResultPruner {
    /// Rewrite every over-budget tool result outside the recent window.
    /// Returns how many were rewritten.
    ///
    /// Idempotent: the configuration is validated so that
    /// `head + marker + tail` is smaller than `threshold_chars`, meaning a
    /// pruned result is strictly under the threshold and a second pass finds
    /// nothing to do. Without that property this would rewrite the same
    /// results on every turn, churning the history and the cache prefix.
    pub fn prune(&self, history: &mut [Message]) -> usize {
        if !self.is_effective() {
            return 0;
        }
        let cutoff = history.len().saturating_sub(self.keep_recent);
        let mut pruned = 0;
        for message in &mut history[..cutoff] {
            for block in &mut message.content {
                if let ContentBlock::ToolResult { content, .. } = block {
                    if let Some(shorter) = self.shrink(content) {
                        *content = shorter;
                        pruned += 1;
                    }
                }
            }
        }
        pruned
    }

    /// Whether this configuration can prune without growing or oscillating.
    ///
    /// A head and tail that do not leave room for the marker under the
    /// threshold would produce output at or above the threshold, so the next
    /// pass would prune it again forever. Rather than silently correcting the
    /// numbers, such a configuration simply does nothing.
    fn is_effective(&self) -> bool {
        self.threshold_chars > 0
            && self
                .head_chars
                .saturating_add(self.tail_chars)
                .saturating_add(PRUNE_MARKER.chars().count())
                < self.threshold_chars
    }

    /// The pruned form of `body`, or `None` if it is already small enough.
    fn shrink(&self, body: &str) -> Option<String> {
        let total = body.chars().count();
        if total <= self.threshold_chars {
            return None;
        }
        // Count in `char`s, never bytes: slicing a UTF-8 string at an
        // arbitrary byte offset panics, and tool output is full of non-ASCII
        // (tree-drawing characters, the elision ellipsis, source in any
        // language). This can still split a multi-char grapheme cluster —
        // acceptable for a display-only truncation.
        let head: String = body.chars().take(self.head_chars).collect();
        let tail: String = body
            .chars()
            .skip(total.saturating_sub(self.tail_chars))
            .collect();
        Some(format!("{head}{PRUNE_MARKER}{tail}"))
    }
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
        let mut split = history.len() - self.keep_recent;
        // Never let the fold boundary orphan a tool_result: a kept
        // `tool_result` whose matching `tool_use` was folded into the recap is
        // an API contract violation (Anthropic/OpenAI/Cohere all 400). Advance
        // the boundary past any message that leads with a tool_result so the
        // first kept message is a clean turn start.
        while split < history.len() && leads_with_tool_result(&history[split]) {
            split += 1;
        }
        if split >= history.len() {
            // The entire tail is tool_result messages (pathological). Skip this
            // round rather than fold everything away; the next turn adds a
            // clean boundary to compact at.
            return None;
        }
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

/// Whether `m` contains a `tool_result` block. Such a message is only valid
/// when the preceding message held the matching `tool_use`, so it can't be the
/// first message after a compaction recap.
fn leads_with_tool_result(m: &Message) -> bool {
    m.content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
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
                // Reasoning is working-out, not conclusion. The recap keeps
                // what the assistant *did*; folding paraphrased thinking into
                // it would put unsigned text where a signed block used to be.
                ContentBlock::Thinking { .. } => {}
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
    fn plan_forced_never_orphans_a_tool_result() {
        let c = Compactor {
            trigger_tokens: 1_000_000,
            keep_recent: 2,
        };
        // A tool-heavy tail: the natural split (len-2 = index 2) lands on the
        // User(tool_result) whose ToolUse is at index 1 — folding there would
        // orphan the result. The boundary must advance past it.
        let history = vec![
            Message::user_text("do a thing"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
            }]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
            Message::assistant(vec![ContentBlock::text("done")]),
        ];
        let plan = c.plan_forced(&history).unwrap();
        // Folded past the tool_result (index 2), so the first kept message is
        // the clean assistant "done" at index 3 — no orphaned result.
        assert_eq!(plan.replaced, 3);
        assert!(!leads_with_tool_result(&history[plan.replaced]));
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

    #[test]
    fn would_trim_agrees_with_trim_even_when_eliding_makes_it_longer() {
        let b = ToolOutputBudget::new(4);
        // Five one-character lines: the marker is longer than the line it
        // replaces, so the trimmed form is *bigger* than the input. A caller
        // comparing lengths would conclude nothing was dropped — and skip
        // spilling a result that really did lose a line.
        let body = "a\nb\nc\nd\ne";
        let out = b.trim(body);
        assert!(b.would_trim(body));
        assert!(out.len() > body.len(), "precondition for the trap");
        assert!(!out.contains('c'), "the middle line really was dropped");

        let fits = "a\nb\nc";
        assert!(!b.would_trim(fits));
        assert_eq!(b.trim(fits), fits);
    }

    fn tool_result(body: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: body.into(),
                is_error: false,
            }],
        }
    }

    fn result_body(m: &Message) -> String {
        match &m.content[0] {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    /// `keep_recent: 0` so the fixtures don't all have to be padded past the
    /// recent window; the window itself is covered separately below.
    fn pruner() -> ToolResultPruner {
        ToolResultPruner {
            threshold_chars: 200,
            head_chars: 50,
            tail_chars: 20,
            keep_recent: 0,
        }
    }

    #[test]
    fn prunes_an_oversized_result_to_head_marker_and_tail() {
        let body = "x".repeat(5000);
        let mut history = vec![tool_result(&body)];
        assert_eq!(pruner().prune(&mut history), 1);
        let out = result_body(&history[0]);
        assert!(out.starts_with(&"x".repeat(50)));
        assert!(out.ends_with(&"x".repeat(20)));
        assert!(out.contains("pruned"));
        assert!(out.chars().count() < 200, "must land under the threshold");
    }

    #[test]
    fn a_result_under_the_threshold_is_left_alone() {
        let mut history = vec![tool_result("short output")];
        assert_eq!(pruner().prune(&mut history), 0);
        assert_eq!(result_body(&history[0]), "short output");
    }

    #[test]
    fn pruning_is_idempotent() {
        let mut history = vec![tool_result(&"y".repeat(5000))];
        let p = pruner();
        assert_eq!(p.prune(&mut history), 1);
        let once = result_body(&history[0]);
        // A second pass must find nothing: head + marker + tail is under the
        // threshold by construction. Otherwise every turn would rewrite the
        // same results and invalidate the cached prefix each time.
        assert_eq!(p.prune(&mut history), 0);
        assert_eq!(result_body(&history[0]), once);
    }

    #[test]
    fn recent_results_survive_because_the_model_is_still_using_them() {
        let big = "z".repeat(5000);
        let mut history = vec![tool_result(&big), tool_result(&big), tool_result(&big)];
        let p = ToolResultPruner {
            keep_recent: 2,
            ..pruner()
        };
        assert_eq!(p.prune(&mut history), 1, "only the oldest is prunable");
        assert_eq!(result_body(&history[1]), big);
        assert_eq!(result_body(&history[2]), big);
    }

    #[test]
    fn pruning_a_multibyte_body_does_not_panic_or_corrupt() {
        // Byte-slicing this at char 50 would panic; tool output is full of
        // box-drawing characters and non-Latin source.
        let body = "★日本語テキスト".repeat(500);
        let mut history = vec![tool_result(&body)];
        assert_eq!(pruner().prune(&mut history), 1);
        let out = result_body(&history[0]);
        assert!(out.starts_with('★'));
        assert!(out.chars().count() < 200);
    }

    #[test]
    fn a_configuration_that_cannot_shrink_does_nothing() {
        // head + tail exceeds the threshold, so "pruning" would grow the
        // result and re-trigger forever. Refuse rather than oscillate.
        let p = ToolResultPruner {
            threshold_chars: 100,
            head_chars: 90,
            tail_chars: 90,
            keep_recent: 0,
        };
        let body = "q".repeat(5000);
        let mut history = vec![tool_result(&body)];
        assert_eq!(p.prune(&mut history), 0);
        assert_eq!(result_body(&history[0]), body);
    }

    #[test]
    fn pruning_reclaims_enough_to_matter() {
        // A long session, where the protected recent window is a small
        // fraction of history — the case pruning exists for. With only a
        // handful of messages the default `keep_recent: 6` correctly protects
        // most of them and there is little to reclaim.
        let big = "w".repeat(40_000);
        let mut history: Vec<Message> = (0..30).map(|_| tool_result(&big)).collect();
        let before = estimate_history_tokens(&history, None);
        let pruned = ToolResultPruner::default().prune(&mut history);
        let after = estimate_history_tokens(&history, None);
        assert_eq!(pruned, 24, "all but the recent window should prune");
        assert!(
            after < before / 2,
            "expected a large reduction, {before} -> {after}"
        );
    }
}
