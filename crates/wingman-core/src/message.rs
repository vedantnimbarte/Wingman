use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A content block inside a message. Mirrors Anthropic's block model so
/// that the most expressive provider can be a 1:1 mapping; other providers
/// translate (e.g. OpenAI's `tool_calls`/`tool` role messages) at their
/// adapter boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        /// base64-encoded image data
        data: String,
        /// MIME type: "image/jpeg", "image/png", "image/gif", "image/webp"
        media_type: String,
    },
    /// A reasoning block emitted by a thinking-capable model.
    ///
    /// These are **not** decoration. Anthropic rejects a tool-use turn whose
    /// preceding assistant message dropped its thinking blocks, so the loop has
    /// to carry them through history verbatim — `signature` is the provider's
    /// integrity tag over `text` and must survive the round trip byte-for-byte.
    ///
    /// `redacted` marks a block whose text the provider encrypted; it carries
    /// no readable content but still has to be echoed back.
    Thinking {
        text: String,
        /// Provider integrity tag. Absent for providers that don't sign
        /// (Gemini) and for summary-only reasoning (OpenAI).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    pub fn thinking(text: impl Into<String>, signature: Option<String>) -> Self {
        Self::Thinking {
            text: text.into(),
            signature,
            redacted: false,
        }
    }

    /// True for blocks the model produced as reasoning rather than as its
    /// answer. Used by anything that renders or summarises a message and
    /// should not treat reasoning as visible output.
    pub fn is_thinking(&self) -> bool {
        matches!(self, Self::Thinking { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(s: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(s)],
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        // Tool results are carried by a user-role message (Anthropic convention).
        Self {
            role: Role::User,
            content: results,
        }
    }
}
