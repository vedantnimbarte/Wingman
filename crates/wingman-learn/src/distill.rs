//! Post-session distillation: after a session, ask a (fast) model to extract
//! durable, project-specific facts worth remembering — build quirks,
//! conventions, gotchas — and stage them in a review file rather than writing
//! them silently into trusted memory. The user promotes the good ones.
//!
//! The model call goes through `wingman_core::complete_text` so this stays
//! provider-agnostic (no dep on `wingman-providers`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wingman_core::{
    complete_text, CompletionRequest, ContentBlock, Message, Provider, Role,
};

/// Cap the transcript we send to the model. Distillation is a cheap side call;
/// the tail of the session holds the freshest, most relevant facts.
const MAX_TRANSCRIPT_CHARS: usize = 24_000;

const DISTILL_SYSTEM: &str = "\
You extract durable, project-specific facts from a coding session transcript — \
things that would help on a FUTURE session in the same repo. Good facts: build/test \
commands and their quirks, environment variables required, conventions the project \
follows, non-obvious gotchas, where key things live. \
Ignore anything one-off, generic, or already obvious from reading the code. \
Output one fact per line, plain text, no numbering or bullets. \
If there is nothing durable worth saving, output the single word NONE.";

/// Flatten a message history into a compact transcript, keeping only user and
/// assistant text and trimming to the last [`MAX_TRANSCRIPT_CHARS`] chars.
pub fn render_transcript(messages: &[Message]) -> String {
    let mut s = String::new();
    for m in messages {
        let who = match m.role {
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
        };
        for b in &m.content {
            if let ContentBlock::Text { text } = b {
                let t = text.trim();
                if !t.is_empty() {
                    s.push_str(who);
                    s.push_str(": ");
                    s.push_str(t);
                    s.push('\n');
                }
            }
        }
    }
    if s.len() > MAX_TRANSCRIPT_CHARS {
        // Keep the tail (most recent), on a char boundary.
        let cut = s.len() - MAX_TRANSCRIPT_CHARS;
        let cut = (cut..s.len())
            .find(|&i| s.is_char_boundary(i))
            .unwrap_or(s.len());
        s = s[cut..].to_string();
    }
    s
}

/// Parse the model's line-per-fact output into clean fact strings. Drops the
/// `NONE` sentinel, blank lines, and any accidental bullet/number prefixes.
pub fn parse_facts(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')')
            .trim();
        if line.is_empty() || line.eq_ignore_ascii_case("none") {
            continue;
        }
        out.push(line.to_string());
    }
    out
}

/// Review file staging distilled facts under the project's `.wingman/`.
pub struct PendingStore {
    path: PathBuf,
}

impl PendingStore {
    pub fn new(project_root: &Path) -> Self {
        Self {
            path: project_root.join(".wingman").join("pending-memories.md"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Currently-staged fact lines (the `- ` checklist items), for dedup and
    /// display.
    pub fn load(&self) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("- [ ] "))
            .map(|s| s.to_string())
            .collect()
    }

    /// Append `facts` not already present. Returns how many were newly added.
    pub fn append(&self, facts: &[String]) -> std::io::Result<usize> {
        let existing = self.load();
        let fresh: Vec<&String> = facts
            .iter()
            .filter(|f| !existing.iter().any(|e| e == *f))
            .collect();
        if fresh.is_empty() {
            return Ok(0);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        if existing.is_empty() && !self.path.exists() {
            body.push_str(
                "# Pending memories (distilled from sessions)\n\n\
                 Review these; promote the good ones with `save_memory` / `/remember`, then \
                 delete them here. Wingman does not trust this file as memory.\n\n",
            );
        }
        for f in fresh.iter() {
            body.push_str("- [ ] ");
            body.push_str(f);
            body.push('\n');
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(body.as_bytes())?;
        Ok(fresh.len())
    }
}

/// Distill durable facts from `messages` and stage them in the project's
/// pending-review file. Returns the number of newly-staged facts.
pub async fn distill_session(
    provider: &Arc<dyn Provider>,
    model: &str,
    messages: &[Message],
    project_root: &Path,
) -> crate::Result<usize> {
    let transcript = render_transcript(messages);
    if transcript.trim().len() < 200 {
        return Ok(0); // too little to distill
    }
    let mut req = CompletionRequest::new(model);
    req.system = Some(DISTILL_SYSTEM.to_string());
    req.max_tokens = 512;
    req.temperature = Some(0.0);
    req.messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("Session transcript:\n\n{transcript}"),
        }],
    }];
    let output = complete_text(provider.as_ref(), req)
        .await
        .map_err(|e| crate::LearnError::Other(format!("distill completion failed: {e}")))?;
    let facts = parse_facts(&output);
    if facts.is_empty() {
        return Ok(0);
    }
    let store = PendingStore::new(project_root);
    let n = store
        .append(&facts)
        .map_err(|e| crate::LearnError::Other(format!("write pending file: {e}")))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_facts_strips_prefixes_and_none() {
        let out = "- Tests need WINGMAN_TEST=1\n2. Run `cargo test -p x`\nNONE\n\n* Uses blake3";
        let facts = parse_facts(out);
        assert_eq!(
            facts,
            vec![
                "Tests need WINGMAN_TEST=1".to_string(),
                "Run `cargo test -p x`".to_string(),
                "Uses blake3".to_string(),
            ]
        );
        assert!(parse_facts("NONE").is_empty());
    }

    #[test]
    fn pending_store_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let store = PendingStore::new(dir.path());
        assert_eq!(store.append(&["a".into(), "b".into()]).unwrap(), 2);
        // "a" already present; only "c" is new.
        assert_eq!(store.append(&["a".into(), "c".into()]).unwrap(), 1);
        let loaded = store.load();
        assert_eq!(loaded, vec!["a", "b", "c"]);
    }

    #[test]
    fn render_transcript_keeps_tail_within_cap() {
        let big = "x".repeat(MAX_TRANSCRIPT_CHARS * 2);
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: big }],
        }];
        let t = render_transcript(&messages);
        assert!(t.len() <= MAX_TRANSCRIPT_CHARS);
    }
}
