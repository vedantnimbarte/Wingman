use serde::{Deserialize, Serialize};

/// Wire-level description of a tool. JSON-schema describes the input. The
/// `Tool` trait that implements behavior lives in `wingman-tools` to keep
/// `wingman-core` free of any IO concerns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the input object. Providers translate to their
    /// preferred wire format (Anthropic accepts JSON Schema directly).
    pub input_schema: serde_json::Value,
}
