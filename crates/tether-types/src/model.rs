//! Model requests and responses in Tether's own shape. Adapters translate to
//! and from provider formats; Tether never alters what the agent sent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub provider: String,
    pub model: String,
    /// Message history as blob hashes, one per message, so a growing history
    /// costs one new blob per step rather than a copy of everything so far.
    pub messages: Vec<String>,
    /// Tools offered to the model on this turn, by fully qualified name.
    pub tools: Vec<String>,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// Blob hash of the response message.
    pub message: String,
    pub tool_calls: Vec<crate::tool::ToolCall>,
    pub usage: Usage,
    /// Why the model stopped, in the provider's words: `end_turn`,
    /// `tool_use`, `max_tokens`, `refusal` and so on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub latency_ms: u64,
    /// Set when the response was streamed and collapsed into this one event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default)]
    pub stream: bool,
    pub provider_request_id: Option<String>,
    /// Blob hash of the raw provider body, for diagnosing format changes.
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}
