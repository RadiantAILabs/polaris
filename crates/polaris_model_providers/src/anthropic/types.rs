//! Anthropic Messages API types.
//!
//! These types match the Anthropic API specification.
//! See: <https://docs.anthropic.com/en/api/messages>

use crate::schema::normalize_schema_for_strict_mode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─────────────────────────────────────────────────────────────────────────────
// Request Types
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for the Messages API.
#[derive(Debug, Clone, Serialize)]
pub struct CreateMessageRequest {
    /// The model to use.
    pub model: String,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Input messages.
    pub messages: Vec<MessageParam>,
    /// System prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Tool definitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    /// How to choose tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceParam>,
    /// Temperature for sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// Structured output format (beta).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<OutputFormat>,
    /// Whether to stream the response via SSE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlockParam>,
}

/// Content block in a request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockParam {
    /// Text content.
    Text {
        /// The text content.
        text: String,
    },
    /// Image content.
    Image {
        /// Image source.
        source: ImageSource,
    },
    /// Tool use block (for assistant messages).
    ToolUse {
        /// Tool call ID.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input.
        input: Value,
    },
    /// Tool result block (for user messages).
    ToolResult {
        /// Tool use ID this result is for.
        tool_use_id: String,
        /// Result content (can be string or array).
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ToolResultContent>,
        /// Whether this is an error result.
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Thinking block (for assistant messages with extended thinking).
    Thinking {
        /// The thinking content.
        thinking: String,
        /// Signature for verification.
        signature: String,
    },
}

/// Content block allowed in tool results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultBlock {
    Text { text: String },
    Image { source: ImageSource },
}

/// Tool result content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

/// Supported image media types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMediaType {
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/webp")]
    Webp,
}

/// Image source for image blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    /// Base64 encoded image.
    Base64 {
        /// Media type.
        media_type: ImageMediaType,
        /// Base64 encoded data.
        data: String,
    },
    /// URL source.
    Url {
        /// Image URL.
        url: String,
    },
}

/// Tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Tool description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for input.
    pub input_schema: Value,
    /// Enable strict mode (beta).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Tool choice configuration.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoiceParam {
    /// Model decides whether to use tools.
    Auto {
        /// Disable parallel tool use.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must use at least one tool.
    Any {
        /// Disable parallel tool use.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must use a specific tool.
    Tool {
        /// Tool name.
        name: String,
        /// Disable parallel tool use.
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    /// Model must not use tools.
    None,
}

/// Structured output format (beta feature).
#[derive(Debug, Clone, Serialize)]
pub struct OutputFormat {
    /// Type is always `json_schema`.
    #[serde(rename = "type")]
    format_type: String,
    /// The JSON schema.
    schema: Value,
}

impl OutputFormat {
    /// Creates a new JSON schema output format.
    ///
    /// This function automatically normalizes the schema to comply with Anthropic's requirements.
    #[must_use]
    pub fn new(schema: Value) -> Self {
        Self {
            format_type: "json_schema".to_string(),
            schema: normalize_schema_for_strict_mode(schema),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Response Types
// ─────────────────────────────────────────────────────────────────────────────

/// Response from the Messages API.
#[derive(Debug, Clone, Deserialize)]
#[expect(dead_code, reason = "fields used for deserialization completeness")]
pub struct MessageResponse {
    /// Unique message ID.
    pub id: String,
    /// Always "message".
    #[serde(rename = "type")]
    pub message_type: String,
    /// Always "assistant".
    pub role: String,
    /// Generated content.
    pub content: Vec<ContentBlock>,
    /// Model used.
    pub model: String,
    /// Reason generation stopped.
    pub stop_reason: StopReason,
    /// Stop sequence if applicable.
    pub stop_sequence: Option<String>,
    /// Token usage.
    pub usage: UsageResponse,
}

/// Content block in a response.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text content.
    Text {
        /// The text.
        text: String,
    },
    /// Tool use request.
    ToolUse {
        /// Tool call ID.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input.
        input: Value,
    },
    /// Thinking content.
    Thinking {
        /// The thinking text.
        thinking: String,
        /// Signature.
        signature: String,
    },
    /// Redacted thinking.
    RedactedThinking {
        /// Redacted data.
        #[expect(dead_code, reason = "field used for deserialization completeness")]
        data: String,
    },
}

/// Reason why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Natural end of turn.
    EndTurn,
    /// Hit max tokens.
    MaxTokens,
    /// Hit stop sequence.
    StopSequence,
    /// Tool use requested.
    ToolUse,
    /// Turn paused.
    PauseTurn,
    /// Refusal.
    Refusal,
}

/// Token usage information.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageResponse {
    /// Input tokens used.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
    /// Cache creation tokens.
    #[serde(default)]
    #[expect(dead_code, reason = "field used for deserialization completeness")]
    pub cache_creation_input_tokens: u64,
    /// Cache read tokens.
    #[serde(default)]
    #[expect(dead_code, reason = "field used for deserialization completeness")]
    pub cache_read_input_tokens: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming Event Types
// ─────────────────────────────────────────────────────────────────────────────

/// A raw SSE event from the Anthropic streaming API.
///
/// These are deserialized from the `data:` field of each SSE frame and then
/// converted into Polaris [`StreamEvent`](polaris_models::llm::StreamEvent)s.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawStreamEvent {
    /// Initial message metadata and input usage.
    #[serde(rename = "message_start")]
    MessageStart {
        /// Partial message with usage information.
        message: MessageStartPayload,
    },
    /// A new content block begins.
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        /// Block index.
        index: u32,
        /// Block type and metadata.
        content_block: StreamContentBlock,
    },
    /// Incremental content for a block.
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        /// Block index.
        index: u32,
        /// The delta payload.
        delta: StreamDelta,
    },
    /// A content block is complete.
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        /// Block index.
        index: u32,
    },
    /// Message-level delta with stop reason and output usage.
    #[serde(rename = "message_delta")]
    MessageDelta {
        /// Stop reason and related metadata.
        delta: MessageDeltaPayload,
        /// Output token count at this point. May be absent in some API responses.
        usage: Option<MessageDeltaUsage>,
    },
    /// Stream is complete.
    #[serde(rename = "message_stop")]
    MessageStop,
    /// Keep-alive ping.
    #[serde(rename = "ping")]
    Ping,
    /// Server-side error.
    #[serde(rename = "error")]
    Error {
        /// Error details.
        error: StreamErrorInfo,
    },
}

/// Payload for the `message_start` event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageStartPayload {
    /// Token usage for the input.
    pub(crate) usage: UsageResponse,
}

/// Content block types that can appear in streaming responses.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StreamContentBlock {
    /// Text block.
    #[serde(rename = "text")]
    Text {
        /// Initial text (usually empty).
        #[expect(dead_code, reason = "field used for deserialization completeness")]
        text: String,
    },
    /// Tool use block.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Tool call ID.
        id: String,
        /// Tool name.
        name: String,
        /// Initial input (usually empty string or empty object).
        #[expect(dead_code, reason = "field used for deserialization completeness")]
        input: Value,
    },
    /// Thinking block.
    #[serde(rename = "thinking")]
    Thinking {
        /// Initial thinking text (usually empty).
        #[expect(dead_code, reason = "field used for deserialization completeness")]
        thinking: String,
    },
    /// Redacted thinking block.
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        /// Redacted data.
        #[expect(dead_code, reason = "field used for deserialization completeness")]
        data: String,
    },
}

/// Delta payload variants for `content_block_delta` events.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum StreamDelta {
    /// Text content fragment.
    #[serde(rename = "text_delta")]
    Text {
        /// The text fragment.
        text: String,
    },
    /// Tool call input JSON fragment.
    #[serde(rename = "input_json_delta")]
    InputJson {
        /// Partial JSON string for tool arguments.
        partial_json: String,
    },
    /// Thinking content fragment.
    #[serde(rename = "thinking_delta")]
    Thinking {
        /// The thinking text fragment.
        thinking: String,
    },
    /// Signature for thinking block verification.
    ///
    /// Sent just before `content_block_stop` for thinking blocks.
    #[serde(rename = "signature_delta")]
    Signature {
        /// The verification signature.
        signature: String,
    },
}

/// Payload for the `message_delta` event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageDeltaPayload {
    /// Reason the model stopped generating.
    pub(crate) stop_reason: StopReason,
}

/// Usage information in the `message_delta` event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageDeltaUsage {
    /// Output tokens generated so far.
    pub(crate) output_tokens: u64,
}

/// Error information from a streaming error event.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StreamErrorInfo {
    /// Error type (e.g. `overloaded_error`).
    #[serde(rename = "type")]
    pub(crate) error_type: String,
    /// Human-readable error message.
    pub(crate) message: String,
}
