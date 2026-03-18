//! Core types for LLM generation requests and responses.

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;

// ─────────────────────
// Request / Response
// ─────────────────────

/// A generation request to a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmRequest {
    /// System prompt for the model.
    pub system: Option<String>,
    /// The messages to send to the model.
    pub messages: Vec<Message>,
    /// Available tools the model can call.
    pub tools: Option<Vec<ToolDefinition>>,
    /// How the model should choose tools.
    pub tool_choice: Option<ToolChoice>,
    /// JSON Schema for structured output (optional).
    ///
    /// When provided, the model will generate output conforming to this schema.
    /// This is set automatically by `Llm::generate_structured()`.
    pub output_schema: Option<Value>,
}

impl LlmRequest {
    /// Returns `true` if any message contains a [`ToolCall`] or [`ToolResult`] block.
    #[must_use]
    pub fn contains_tool_blocks(&self) -> bool {
        self.messages.iter().any(|msg| match msg {
            Message::User { content } => content
                .iter()
                .any(|b| matches!(b, UserBlock::ToolResult(_))),
            Message::Assistant { content, .. } => content
                .iter()
                .any(|b| matches!(b, AssistantBlock::ToolCall(_))),
        })
    }
}

/// The reason generation stopped.
///
/// All variants represent a successful generation: a [`LlmResponse`]
/// was produced and [`content`](LlmResponse::content) may be usable.
///
/// Some providers may return additional provider-specific reasons which do not
/// fall under the existing classification in the [`Other`](Self::Other) variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model reached a natural stopping point.
    EndTurn,
    /// Output was truncated because `max_tokens` was reached.
    MaxOutputTokens,
    /// A caller-provided stop sequence was generated.
    StopSequence,
    /// The model invoked one or more tools.
    ToolUse,
    /// Output was interrupted by a safety or content filter.
    ///
    /// Partial content may still be present in
    /// [`LlmResponse::content`].
    ///
    /// When the provider rejects the request entirely and produces no content,
    /// [`GenerationError::Refusal`] should be used instead.
    ///
    /// [`GenerationError::Refusal`]: super::GenerationError::Refusal
    ContentFilter,
    /// A provider-specific reason (e.g. `"pause_turn"`).
    #[serde(untagged)]
    Other(String),
}

/// A generation response from a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// The generated content blocks.
    pub content: Vec<AssistantBlock>,
    /// Token usage information.
    pub usage: Usage,
    /// The reason generation stopped.
    pub stop_reason: StopReason,
}

impl LlmResponse {
    /// Returns all text content blocks concatenated into a single string.
    ///
    /// This is a convenience method for the common case of extracting
    /// text from a response. If multiple text blocks exist, they are
    /// concatenated together.
    ///
    /// Returns an empty string if no text content is found.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Text(block) => Some(block.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Returns `true` if the response contains any tool calls.
    #[must_use]
    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, AssistantBlock::ToolCall(_)))
    }

    /// Returns all tool calls contained in the response.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }
}

/// Token usage information.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Number of tokens in the input.
    pub input_tokens: Option<u64>,
    /// Number of tokens in the output.
    pub output_tokens: Option<u64>,
    /// Total tokens (input + output).
    pub total_tokens: Option<u64>,
}

// ─────────────────────
// Messages
// ─────────────────────

/// An input (user) or output (assistant) message in a conversation. Each message contains at least one content block.
///
/// Since models may not support all content types, the conversion from `Message` to provider-specific formats may be lossy (e.g., images may be omitted for text-only models).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// A message from the user.
    User {
        /// The content blocks of the user message.
        content: Vec<UserBlock>,
    },
    /// A message from the assistant.
    Assistant {
        /// Optional identifier for this assistant.
        id: Option<String>,
        /// The content blocks of the assistant message.
        content: Vec<AssistantBlock>,
    },
}

impl Message {
    /// Creates a user message with text content.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![UserBlock::Text(TextBlock { text: text.into() })],
        }
    }

    /// Creates an assistant message with text content.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            id: None,
            content: vec![AssistantBlock::Text(TextBlock { text: text.into() })],
        }
    }

    /// Creates an assistant reasoning message with the given thought content.
    #[must_use]
    pub fn reasoning(thought: impl Into<String>) -> Self {
        Self::Assistant {
            id: None,
            content: vec![AssistantBlock::reasoning(thought)],
        }
    }

    /// Creates an assistant message with text content and an ID.
    #[must_use]
    pub fn assistant_with_id(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Assistant {
            id: Some(id.into()),
            content: vec![AssistantBlock::Text(TextBlock { text: text.into() })],
        }
    }

    /// Creates an assistant message with a single tool call.
    #[must_use]
    pub fn assistant_tool_call(call: ToolCall) -> Self {
        Self::Assistant {
            id: None,
            content: vec![AssistantBlock::ToolCall(call)],
        }
    }

    /// Creates a user message with a tool result.
    #[must_use]
    pub fn tool_result(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![UserBlock::tool_result(id, content)],
        }
    }

    /// Creates a user message with a tool error result.
    #[must_use]
    pub fn tool_error(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![UserBlock::tool_error(id, content)],
        }
    }

    /// Creates a user message with a tool result that includes a call ID.
    #[must_use]
    pub fn tool_result_with_call_id(
        id: impl Into<String>,
        call_id: impl Into<String>,
        content: ToolResultContent,
    ) -> Self {
        Self::User {
            content: vec![UserBlock::tool_result_with_call_id(id, call_id, content)],
        }
    }

    /// Creates a user message with a tool error result that includes a call ID.
    #[must_use]
    pub fn tool_error_with_call_id(
        id: impl Into<String>,
        call_id: impl Into<String>,
        content: ToolResultContent,
    ) -> Self {
        Self::User {
            content: vec![UserBlock::tool_error_with_call_id(id, call_id, content)],
        }
    }
}

// ─────────────────────
// Content Blocks
// ─────────────────────

/// Plain text content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    /// The text content.
    pub text: String,
}

impl From<String> for TextBlock {
    fn from(text: String) -> Self {
        Self { text }
    }
}

impl From<&str> for TextBlock {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
        }
    }
}

/// Content that can appear in a user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserBlock {
    /// Plain text content.
    Text(TextBlock),
    /// Image content for vision models.
    Image(ImageBlock),
    /// Audio content for speech models.
    Audio(AudioBlock),
    /// Document content (PDF, code, etc.).
    Document(DocumentBlock),
    /// A tool call result from execution.
    ToolResult(ToolResult),
}

impl UserBlock {
    /// Creates a text content block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock { text: text.into() })
    }

    /// Creates an image content block from base64-encoded data.
    #[must_use]
    pub fn image_base64(data: impl Into<String>, media_type: ImageMediaType) -> Self {
        Self::Image(ImageBlock {
            data: DocumentSource::Base64(data.into()),
            media_type,
            additional_params: None,
        })
    }

    /// Creates an audio content block from base64-encoded data.
    #[must_use]
    pub fn audio_base64(data: impl Into<String>, media_type: AudioMediaType) -> Self {
        Self::Audio(AudioBlock {
            data: DocumentSource::Base64(data.into()),
            media_type,
            additional_params: None,
        })
    }

    /// Creates a document content block from base64-encoded data.
    #[must_use]
    pub fn document_base64(
        name: impl Into<String>,
        data: impl Into<String>,
        media_type: DocumentMediaType,
    ) -> Self {
        Self::Document(DocumentBlock {
            name: name.into(),
            data: DocumentSource::Base64(data.into()),
            media_type,
            additional_params: None,
        })
    }

    /// Creates a tool result content block.
    #[must_use]
    pub fn tool_result(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::ToolResult(ToolResult {
            id: id.into(),
            call_id: None,
            content,
            status: ToolResultStatus::Success,
        })
    }

    /// Creates an error tool result content block.
    #[must_use]
    pub fn tool_error(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::ToolResult(ToolResult {
            id: id.into(),
            call_id: None,
            content,
            status: ToolResultStatus::Error,
        })
    }

    /// Creates a tool result content block with a call ID.
    #[must_use]
    pub fn tool_result_with_call_id(
        id: impl Into<String>,
        call_id: impl Into<String>,
        content: ToolResultContent,
    ) -> Self {
        Self::ToolResult(ToolResult {
            id: id.into(),
            call_id: Some(call_id.into()),
            content,
            status: ToolResultStatus::Success,
        })
    }

    /// Creates an error tool result content block with a call ID.
    #[must_use]
    pub fn tool_error_with_call_id(
        id: impl Into<String>,
        call_id: impl Into<String>,
        content: ToolResultContent,
    ) -> Self {
        Self::ToolResult(ToolResult {
            id: id.into(),
            call_id: Some(call_id.into()),
            content,
            status: ToolResultStatus::Error,
        })
    }
}

/// Content that can appear in an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantBlock {
    /// Plain text content.
    Text(TextBlock),
    /// A tool call request from the model.
    ToolCall(ToolCall),
    /// Reasoning/thinking content from the model.
    Reasoning(ReasoningBlock),
}

impl AssistantBlock {
    /// Creates a text content block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextBlock { text: text.into() })
    }

    /// Creates a tool call content block.
    #[must_use]
    pub fn tool_call(call: ToolCall) -> Self {
        Self::ToolCall(call)
    }

    /// Creates a reasoning content block.
    #[must_use]
    pub fn reasoning(reasoning: impl Into<String>) -> Self {
        Self::Reasoning(ReasoningBlock {
            id: None,
            reasoning: vec![reasoning.into()],
            signature: None,
        })
    }

    /// Creates a reasoning content block with a signature.
    #[must_use]
    pub fn reasoning_with_signature(
        reasoning: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self::Reasoning(ReasoningBlock {
            id: None,
            reasoning: vec![reasoning.into()],
            signature: Some(signature.into()),
        })
    }
}

/// Image content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBlock {
    /// The image data.
    pub data: DocumentSource,
    /// The image format.
    pub media_type: ImageMediaType,
    /// Provider-specific parameters.
    pub additional_params: Option<Value>,
}

/// Supported image formats. A provider may support a subset of these formats.
#[expect(missing_docs, reason = "variants are self-explanatory format names")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageMediaType {
    JPEG,
    PNG,
    GIF,
    WEBP,
    HEIC,
    HEIF,
    SVG,
}

/// Audio content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioBlock {
    /// The audio data.
    pub data: DocumentSource,
    /// The audio format.
    pub media_type: AudioMediaType,
    /// Provider-specific parameters.
    pub additional_params: Option<Value>,
}

/// Supported audio formats. A provider may support a subset of these formats.
#[expect(missing_docs, reason = "variants are self-explanatory format names")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioMediaType {
    WAV,
    MP3,
    AIFF,
    AAC,
    OGG,
    FLAC,
}

/// Document content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentBlock {
    /// The document name.
    pub name: String,
    /// The document data.
    pub data: DocumentSource,
    /// The document format.
    pub media_type: DocumentMediaType,
    /// Provider-specific parameters.
    pub additional_params: Option<Value>,
}

/// Supported document formats. A provider may support a subset of these formats.
#[expect(missing_docs, reason = "variants are self-explanatory format names")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocumentMediaType {
    PDF,
    TXT,
    HTML,
    MARKDOWN,
    CSV,
}

/// Reasoning/thinking content from extended thinking models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningBlock {
    /// Provider-assigned identifier for this reasoning block.
    pub id: Option<String>,
    /// The reasoning steps or thoughts.
    pub reasoning: Vec<String>,
    /// Signature for verification (required by some providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Source of binary data for media content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocumentSource {
    /// Base64-encoded data.
    Base64(String),
}

// ─────────────────────
// Tool Calling
// ─────────────────────

/// Definition of a tool that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Name of the tool (e.g., `get_weather`, `search_database`).
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema defining the tool's parameters.
    ///
    /// This should be an object schema with properties defining each parameter.
    /// Example:
    /// ```json
    /// {
    ///   "type": "object",
    ///   "properties": {
    ///     "city": {"type": "string", "description": "City name"},
    ///     "units": {"type": "string", "enum": ["celsius", "fahrenheit"]}
    ///   },
    ///   "required": ["city"]
    /// }
    /// ```
    pub parameters: Value,
}

/// Controls how the model should select tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call tools or respond with text.
    Auto,
    /// Model must call at least one tool.
    Required,
    /// Model must call this specific tool.
    Specific(String),
    /// Model must not call any tools.
    None,
}

/// A tool call request from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Provider-specific call identifier (e.g., for `OpenAI` function calling).
    pub call_id: Option<String>,
    /// The function to call.
    pub function: ToolFunction,
    /// Optional cryptographic signature for verification.
    pub signature: Option<String>,
    /// Provider-specific parameters.
    pub additional_params: Option<Value>,
}

impl ToolCall {
    /// Creates a new tool call with the given ID, function name, and arguments.
    ///
    /// Provider-specific fields (`call_id`, `signature`, `additional_params`)
    /// are set to `None`.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            call_id: None,
            function: ToolFunction {
                name: name.into(),
                arguments,
            },
            signature: None,
            additional_params: None,
        }
    }
}

/// A tool function to be called.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// The name of the function to call.
    pub name: String,
    /// The arguments to pass to the function.
    pub arguments: Value,
}

/// Status of a tool result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    /// The tool executed successfully.
    #[default]
    Success,
    /// The tool encountered an error.
    Error,
}

/// Result of a tool call execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Identifier linking this result to the original tool call.
    pub id: String,
    /// Optional provider-specific call identifier.
    pub call_id: Option<String>,
    /// The result content.
    pub content: ToolResultContent,
    /// Whether this result represents a success or error.
    #[serde(default)]
    pub status: ToolResultStatus,
}

/// Content of a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResultContent {
    /// Text result.
    Text(String),
    /// Image result.
    Image(ImageBlock),
}

// ─────────────────────
// Streaming
// ─────────────────────

/// Information about a content block when it starts streaming.
///
/// Only carries metadata known at block open time. Content arrives
/// via [`ContentBlockDelta`] events after this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlockStartData {
    /// A text content block.
    Text,
    /// A tool call block.
    ToolCall {
        /// Unique identifier for this tool call.
        id: String,
        /// Provider-specific call identifier (e.g., for `OpenAI` function calling).
        call_id: Option<String>,
        /// The name of the tool being invoked.
        name: String,
    },
    /// A reasoning/thinking block.
    Reasoning,
}

/// An incremental content block payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlockDelta {
    /// A chunk of generated text.
    Text(String),
    /// A fragment of tool call arguments as partial JSON.
    ///
    /// Accumulate across deltas and parse on [`StreamEvent::ContentBlockStop`].
    /// Corresponds to [`ToolFunction::arguments`] in the non-streaming response.
    ToolCall {
        /// A fragment of the JSON arguments string.
        arguments: String,
    },
    /// An incremental update to a reasoning block.
    Reasoning(String),
}

impl ContentBlockDelta {
    /// Returns a human-readable label for the variant.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::ToolCall { .. } => "tool_call",
            Self::Reasoning(_) => "reasoning",
        }
    }
}

/// A single incremental event from a streaming LLM response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// A new content block has started.
    ContentBlockStart {
        /// Index of the content block.
        index: u32,
        /// The type of block and its associated metadata.
        block: ContentBlockStartData,
    },

    /// An incremental update to the content block at `index`.
    ContentBlockDelta {
        /// Index of the content block.
        index: u32,
        /// The typed delta payload.
        delta: ContentBlockDelta,
    },

    /// The content block at `index` is complete.
    ///
    /// For tool calls, this signals that the accumulated JSON arguments
    /// are complete and safe to parse.
    ContentBlockStop {
        /// Index of the content block.
        index: u32,
    },

    /// A usage update. May be emitted more than once as the
    /// stream progresses. Each emission reflects totals up to that point.
    ///
    /// Not all providers may emit this event.
    ///
    /// Consumers that only need final usage should use
    /// [`StreamEvent::MessageStop`] and ignore this event.
    MessageDelta {
        /// Cumulative token usage at this point in the stream.
        usage: Usage,
    },

    /// Stream completion.
    MessageStop {
        /// Reason model stopped generating.
        stop_reason: StopReason,
        /// Final token counts for the complete request.
        usage: Usage,
    },
}

/// A boxed stream of [`StreamEvent`]s.
///
/// Returned by streaming methods on [`LlmProvider`](super::provider::LlmProvider),
/// [`Llm`](super::model::Llm), and [`LlmRequestBuilder`](super::builder::LlmRequestBuilder).
pub type LlmStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, super::error::GenerationError>> + Send>>;
