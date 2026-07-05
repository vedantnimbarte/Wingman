use crate::{ContentBlock, Result, Usage};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A streamed text chunk from the assistant. Concatenate to render live.
    TextDelta { text: String },
    /// A fully-assembled tool-use block. Providers buffer partial JSON until
    /// the call is complete, then emit a single event.
    ToolUse { block: ContentBlock },
    /// Cumulative usage so far this turn. Providers may emit this multiple
    /// times (e.g. once at start with input tokens, once at end with output).
    Usage { usage: Usage },
    /// Terminal event. The agent inspects `reason` to decide whether to
    /// continue the loop (e.g. `ToolUse` means run tools and continue).
    Stop { reason: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}

/// Boxed stream of provider events; one per provider response.
pub type ProviderEventStream = BoxStream<'static, Result<StreamEvent>>;
