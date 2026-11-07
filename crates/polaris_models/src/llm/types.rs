//! Core types for LLM generation requests and responses.

use core::num::NonZeroU32;
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
    /// Name of the system prompt variant in use, for observability.
    pub system_prompt_name: Option<String>,
    /// Version of the system prompt variant in use, for observability.
    pub system_prompt_version: Option<String>,
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
    /// Maximum number of tokens the model may generate, when supported.
    ///
    /// `None` leaves the provider's existing output limit unchanged. A response
    /// stopped at this limit reports [`StopReason::MaxOutputTokens`].
    ///
    /// The limit is non-zero by type: a request for zero output tokens has no
    /// useful meaning, and providers reject it. Deserializing a `0` fails.
    pub max_output_tokens: Option<NonZeroU32>,
    /// How much effort the model should spend reasoning, when supported.
    ///
    /// `None` leaves the provider's and the model's own default in place, while
    /// `Some(ReasoningEffort::None)` asks the model to turn reasoning off. A
    /// provider that cannot honour a requested effort fails the request with
    /// [`GenerationError::UnsupportedOperation`](super::GenerationError::UnsupportedOperation)
    /// before sending anything; Polaris never drops the setting silently, but a
    /// server may still ignore a level it does not know. Check
    /// [`Usage::reasoning_output_tokens`] when that matters. See
    /// [`ReasoningEffort`] for the levels and how each provider maps them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Prompt-cache breakpoint markers for this request.
    ///
    /// Provider-agnostic: a provider that supports prompt caching (e.g.
    /// Anthropic) translates these into its native cache-control markers; a
    /// provider that does not simply ignores them. Defaults to disabled.
    #[serde(default)]
    pub cache: CacheControl,
}

/// Declarative prompt-cache breakpoint markers for an [`LlmRequest`].
///
/// The model types stay provider-agnostic: this descriptor names *where* a
/// consumer wants cache breakpoints, and the provider maps them onto its native
/// mechanism (for Anthropic, `cache_control: {type: "ephemeral"}` on the last
/// block of the marked region). The two things consumers reason about are the
/// **stable prefix** (system + tools, re-sent verbatim every call) and one or
/// more **conversation breakpoints** (a stable mid-history point and a rolling
/// recent point — the incremental-caching pattern).
///
/// Construct via [`LlmRequestBuilder`](super::builder::LlmRequestBuilder)'s
/// [`cache_prefix()`](super::builder::LlmRequestBuilder::cache_prefix) /
/// [`cache_breakpoint()`](super::builder::LlmRequestBuilder::cache_breakpoint)
/// verbs rather than computing indices by hand, or build one standalone with
/// [`CacheControl::prefix`] /
/// [`with_breakpoints`](CacheControl::with_breakpoints). [`Default`] is "no
/// caching".
///
/// `#[non_exhaustive]` - breakpoint kinds may be added in the future to
/// construct it through the verbs/constructors above rather than a
/// struct literal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CacheControl {
    /// Cache the stable prefix: the system prompt and tool definitions.
    ///
    /// This is the common case and the dominant win — the fixed prefix is
    /// otherwise re-billed at full price on every call.
    pub prefix: bool,
    /// Indices into [`LlmRequest::messages`] after which to place a cache
    /// breakpoint (the prefix up to and including that message is cached).
    ///
    /// Providers honor a bounded number of breakpoints (Anthropic: 4 total,
    /// counting prefix markers); extras are applied low-to-high and the rest
    /// ignored.
    pub breakpoints: Vec<usize>,
}

impl CacheControl {
    /// A [`CacheControl`] that caches the stable prefix (system prompt + tool
    /// definitions) and nothing else — the common case and the dominant cost
    /// win. Equivalent to the builder's
    /// [`cache_prefix()`](super::builder::LlmRequestBuilder::cache_prefix) verb.
    #[must_use]
    pub fn prefix() -> Self {
        Self {
            prefix: true,
            breakpoints: Vec::new(),
        }
    }

    /// Sets the message breakpoint indices (into [`LlmRequest::messages`]),
    /// replacing any already present, and returns the updated value. Chains
    /// onto [`prefix`](Self::prefix) or [`Default`].
    #[must_use]
    pub fn with_breakpoints(mut self, indices: impl IntoIterator<Item = usize>) -> Self {
        self.breakpoints = indices.into_iter().collect();
        self
    }

    /// Returns `true` when at least one cache breakpoint (prefix or message) is
    /// requested.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.prefix || !self.breakpoints.is_empty()
    }

    /// Returns `true` when no caching is requested (the [`Default`]) — the
    /// negation of [`is_enabled`](Self::is_enabled).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        !self.is_enabled()
    }
}

/// How much effort the model should spend reasoning before it answers.
///
/// A provider-neutral request setting, carried by
/// [`LlmRequest::reasoning_effort`] and set with
/// [`LlmRequestBuilder::reasoning_effort`](super::builder::LlmRequestBuilder::reasoning_effort).
/// Each provider sends it in its own way:
///
/// | Provider | Sent as | Levels it accepts |
/// |----------|---------|-------------------|
/// | Chat Completions | `reasoning_effort` | every level, as the level's name; the server accepts or rejects it |
/// | `OpenAI` (Responses) | `reasoning.effort` | every level, as the level's name |
/// | Anthropic | `output_config.effort` | [`Low`](Self::Low) to [`Max`](Self::Max) |
/// | Bedrock, `openai.*` models | `reasoning.effort` | every level, as the level's name |
/// | Bedrock, Converse | not supported | none |
///
/// The levels are the names servers use, not a promise that every model has
/// that level. A server that does not know a level rejects the request and its
/// error reaches the caller. A provider that has no way to send the setting at
/// all (or no way to send this level) returns
/// [`GenerationError::UnsupportedOperation`](super::GenerationError::UnsupportedOperation)
/// before it sends anything, so a run that pins an effort never runs at a
/// different one without saying so.
///
/// Text converts both ways: [`Display`](core::fmt::Display) writes the name
/// in lower case and [`FromStr`](core::str::FromStr) reads the known names in
/// any case. Other non-empty text becomes [`Other`](Self::Other), so a level
/// that only some servers define can be requested.
///
/// # Examples
///
/// ```
/// use polaris_models::llm::ReasoningEffort;
///
/// assert_eq!("High".parse::<ReasoningEffort>(), Ok(ReasoningEffort::High));
/// assert_eq!(ReasoningEffort::XHigh.to_string(), "xhigh");
/// assert_eq!(
///     "ultra".parse::<ReasoningEffort>(),
///     Ok(ReasoningEffort::Other("ultra".to_owned()))
/// );
/// assert!("  ".parse::<ReasoningEffort>().is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReasoningEffort {
    /// Ask the model not to reason, where it allows that.
    None,
    /// The least reasoning a model offers above none.
    Minimal,
    /// Light reasoning.
    Low,
    /// Moderate reasoning.
    Medium,
    /// Deep reasoning.
    High,
    /// Deeper than [`High`](Self::High), for models that have the level.
    XHigh,
    /// The most reasoning a model offers.
    Max,
    /// A level this enum does not name, sent as written to providers that take
    /// free-form levels.
    ///
    /// Use it for names that are not one of the named levels. Providers decide
    /// by the level's text, so `Other("low")` is sent like [`Low`](Self::Low)
    /// and `Other("none")` is rejected like [`None`](Self::None) where a
    /// provider has a fixed list of levels. Serializing `Other` with empty or
    /// whitespace-only text is an error.
    ///
    /// `Other("high")` sends the same text as [`High`](Self::High) but does not
    /// compare equal to it, and reading the text back always gives the named
    /// variant.
    Other(String),
}

impl ReasoningEffort {
    /// The level's name as servers write it: lower case for the named levels,
    /// the given text for [`Other`](Self::Other).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Other(level) => level,
        }
    }
}

impl core::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text given to [`ReasoningEffort`]'s `FromStr` was empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a reasoning effort cannot be empty")]
pub struct ParseReasoningEffortError {
    _private: (),
}

impl core::str::FromStr for ReasoningEffort {
    type Err = ParseReasoningEffortError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ParseReasoningEffortError { _private: () });
        }
        Ok(match text.to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => Self::Other(text.to_owned()),
        })
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.as_str().trim().is_empty() {
            return Err(serde::ser::Error::custom(
                "a reasoning effort cannot be empty",
            ));
        }
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl LlmRequest {
    /// Returns `true` if any message contains a [`ToolCall`] or [`ToolResult`] block.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{LlmRequest, Message};
    ///
    /// let request = LlmRequest {
    ///     messages: vec![Message::user("Hello")],
    ///     ..Default::default()
    /// };
    /// assert!(!request.contains_tool_blocks());
    /// ```
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
    /// Provider-assigned identifier for this response, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The concrete model that produced this response, when reported.
    ///
    /// May differ from the requested model (e.g. a dated alias resolving
    /// to a specific snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl LlmResponse {
    /// Returns all text content blocks concatenated into a single string.
    ///
    /// This is a convenience method for the common case of extracting
    /// text from a response. If multiple text blocks exist, they are
    /// concatenated together.
    ///
    /// Returns an empty string if no text content is found.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{LlmResponse, AssistantBlock, StopReason, Usage};
    ///
    /// let response = LlmResponse {
    ///     content: vec![AssistantBlock::text("Hello, "), AssistantBlock::text("world!")],
    ///     usage: Usage::default(),
    ///     stop_reason: StopReason::EndTurn,
    ///     id: None,
    ///     model: None,
    /// };
    /// assert_eq!(response.text(), "Hello, world!");
    /// ```
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
///
/// `input_tokens` counts only tokens billed at the full input rate. When prompt
/// caching is in effect, cached prefix tokens are reported separately in
/// [`cache_read_tokens`](Self::cache_read_tokens) (billed at a steep discount)
/// and [`cache_creation_tokens`](Self::cache_creation_tokens) (billed at a small
/// premium the first time a prefix is written). Providers without caching leave
/// the cache fields `None`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Number of full-price (uncached) input tokens.
    pub input_tokens: Option<u64>,
    /// Number of tokens in the output.
    pub output_tokens: Option<u64>,
    /// Total tokens (input + output, including any cached input).
    pub total_tokens: Option<u64>,
    /// Input tokens served from the prompt cache, priced by
    /// [`ModelPricing::cache_read_per_million_usd`](super::ModelPricing::cache_read_per_million_usd).
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to the prompt cache this call.
    ///
    /// Named "creation" to mirror the provider wire field (Anthropic's
    /// `cache_creation_input_tokens`); these are the same tokens priced by
    /// [`ModelPricing::cache_write_per_million_usd`](super::ModelPricing::cache_write_per_million_usd)
    /// ("write" being the billing-tier term for them).
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
    /// Output tokens spent on model reasoning, when the provider reports it
    /// (e.g. `OpenAI` reasoning models via `output_tokens_details.reasoning_tokens`).
    /// Providers that don't report reasoning tokens leave this `None`.
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
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
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::Message;
    ///
    /// let msg = Message::user("What is the weather?");
    /// ```
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
    ///
    /// The result carries no provider call ID, so the `OpenAI` provider
    /// rejects it. Use [`Message::tool_result_for`] instead.
    #[must_use]
    #[deprecated(
        since = "0.8.2",
        note = "drops the provider call ID, which the OpenAI provider requires; use `Message::tool_result_for`"
    )]
    pub fn tool_result(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![UserBlock::ToolResult(ToolResult {
                id: id.into(),
                call_id: None,
                content,
                status: ToolResultStatus::Success,
            })],
        }
    }

    /// Creates a user message with a tool error result.
    ///
    /// The result carries no provider call ID, so the `OpenAI` provider
    /// rejects it. Use [`Message::tool_error_for`] instead.
    #[must_use]
    #[deprecated(
        since = "0.8.2",
        note = "drops the provider call ID, which the OpenAI provider requires; use `Message::tool_error_for`"
    )]
    pub fn tool_error(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![UserBlock::ToolResult(ToolResult {
                id: id.into(),
                call_id: None,
                content,
                status: ToolResultStatus::Error,
            })],
        }
    }

    /// Creates a user message with the successful result of `call`.
    ///
    /// Copies both identifiers from the call, so every provider can link the
    /// result back to it. See [`ToolCall::result`]. If you have only the two
    /// identifiers and not the call, use [`Message::tool_result_with_call_id`].
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{Message, ToolCall, ToolResultContent, ToolResultStatus, UserBlock};
    /// use serde_json::json;
    ///
    /// let mut call = ToolCall::new("fc_1", "get_weather", json!({}));
    /// call.call_id = Some("call_1".to_owned());
    ///
    /// let message = Message::tool_result_for(&call, ToolResultContent::Text("Sunny".into()));
    /// let Message::User { content } = message else {
    ///     unreachable!("tool_result_for builds a user message");
    /// };
    /// let [UserBlock::ToolResult(result)] = content.as_slice() else {
    ///     unreachable!("the message holds one tool result");
    /// };
    /// assert_eq!(result.id, "fc_1");
    /// assert_eq!(result.call_id.as_deref(), Some("call_1"));
    /// assert_eq!(result.status, ToolResultStatus::Success);
    /// ```
    #[must_use]
    pub fn tool_result_for(call: &ToolCall, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![call.result(content)],
        }
    }

    /// Creates a user message with the error result of `call`.
    ///
    /// Copies both identifiers from the call, so every provider can link the
    /// result back to it. See [`ToolCall::error`]. If you have only the two
    /// identifiers and not the call, use [`Message::tool_error_with_call_id`].
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{Message, ToolCall, ToolResultContent, ToolResultStatus, UserBlock};
    /// use serde_json::json;
    ///
    /// let mut call = ToolCall::new("fc_1", "get_weather", json!({}));
    /// call.call_id = Some("call_1".to_owned());
    ///
    /// let message = Message::tool_error_for(&call, ToolResultContent::Text("timed out".into()));
    /// let Message::User { content } = message else {
    ///     unreachable!("tool_error_for builds a user message");
    /// };
    /// let [UserBlock::ToolResult(result)] = content.as_slice() else {
    ///     unreachable!("the message holds one tool result");
    /// };
    /// assert_eq!(result.id, "fc_1");
    /// assert_eq!(result.call_id.as_deref(), Some("call_1"));
    /// assert_eq!(result.status, ToolResultStatus::Error);
    /// ```
    #[must_use]
    pub fn tool_error_for(call: &ToolCall, content: ToolResultContent) -> Self {
        Self::User {
            content: vec![call.error(content)],
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
    ///
    /// The block carries no provider call ID, so the `OpenAI` provider
    /// rejects it. Use [`ToolCall::result`] instead.
    #[must_use]
    #[deprecated(
        since = "0.8.2",
        note = "drops the provider call ID, which the OpenAI provider requires; build the result from the call with `ToolCall::result` or `ToolCall::error`"
    )]
    pub fn tool_result(id: impl Into<String>, content: ToolResultContent) -> Self {
        Self::ToolResult(ToolResult {
            id: id.into(),
            call_id: None,
            content,
            status: ToolResultStatus::Success,
        })
    }

    /// Creates an error tool result content block.
    ///
    /// The block carries no provider call ID, so the `OpenAI` provider
    /// rejects it. Use [`ToolCall::error`] instead.
    #[must_use]
    #[deprecated(
        since = "0.8.2",
        note = "drops the provider call ID, which the OpenAI provider requires; build the result from the call with `ToolCall::result` or `ToolCall::error`"
    )]
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
///
/// Construct via [`ToolDefinition::new`] (and [`with_strict`](Self::with_strict));
/// the struct is `#[non_exhaustive]` so fields may be added without a breaking
/// change, and external callers cannot build it with a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Whether the model provider should enforce this tool's schema in *strict*
    /// mode (constrained decoding that guarantees schema-valid arguments).
    ///
    /// This is the tool author's declared preference. The agent designer may
    /// override it on the tool registry, and the provider applies its own cap
    /// (e.g. Anthropic allows at most 20 strict tools per request, degrading the
    /// overflow to non-strict in registration order). Defaults to `true`.
    #[serde(default = "default_strict")]
    pub strict: bool,
}

/// Default value for [`ToolDefinition::strict`] — strict mode is opt-out.
fn default_strict() -> bool {
    true
}

impl ToolDefinition {
    /// Creates a tool definition with strict mode enabled (the default).
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            strict: true,
        }
    }

    /// Sets whether the provider should enforce this tool's schema in strict mode.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
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
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::ToolCall;
    /// use serde_json::json;
    ///
    /// let call = ToolCall::new("call_1", "get_weather", json!({"city": "London"}));
    /// assert_eq!(call.function.name, "get_weather");
    /// ```
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

    /// Builds the successful result block for this call.
    ///
    /// Copies [`id`](Self::id) and [`call_id`](Self::call_id) into the
    /// result. Providers link a result to its call by different fields:
    /// Anthropic and Bedrock use `id`, while `OpenAI` uses `call_id`. Building
    /// the result from the call keeps both, so the conversation works with
    /// every provider. If you have only the two identifiers and not the call,
    /// use [`UserBlock::tool_result_with_call_id`].
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{ToolCall, ToolResultContent, UserBlock};
    /// use serde_json::json;
    ///
    /// let mut call = ToolCall::new("fc_1", "get_weather", json!({}));
    /// call.call_id = Some("call_1".to_owned());
    ///
    /// let UserBlock::ToolResult(result) = call.result(ToolResultContent::Text("Sunny".into()))
    /// else {
    ///     unreachable!("result builds a tool result block");
    /// };
    /// assert_eq!(result.id, "fc_1");
    /// assert_eq!(result.call_id.as_deref(), Some("call_1"));
    /// ```
    #[must_use]
    pub fn result(&self, content: ToolResultContent) -> UserBlock {
        self.result_block(content, ToolResultStatus::Success)
    }

    /// Builds the error result block for this call.
    ///
    /// Copies both identifiers, like [`ToolCall::result`], and marks the
    /// result as an error. If you have only the two identifiers and not the
    /// call, use [`UserBlock::tool_error_with_call_id`].
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_models::llm::{ToolCall, ToolResultContent, ToolResultStatus, UserBlock};
    /// use serde_json::json;
    ///
    /// let mut call = ToolCall::new("fc_1", "get_weather", json!({}));
    /// call.call_id = Some("call_1".to_owned());
    ///
    /// let UserBlock::ToolResult(result) = call.error(ToolResultContent::Text("timed out".into()))
    /// else {
    ///     unreachable!("error builds a tool result block");
    /// };
    /// assert_eq!(result.id, "fc_1");
    /// assert_eq!(result.call_id.as_deref(), Some("call_1"));
    /// assert_eq!(result.status, ToolResultStatus::Error);
    /// ```
    #[must_use]
    pub fn error(&self, content: ToolResultContent) -> UserBlock {
        self.result_block(content, ToolResultStatus::Error)
    }

    fn result_block(&self, content: ToolResultContent, status: ToolResultStatus) -> UserBlock {
        UserBlock::ToolResult(ToolResult {
            id: self.id.clone(),
            call_id: self.call_id.clone(),
            content,
            status,
        })
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
    /// A cryptographic signature for a reasoning block.
    ///
    /// Some providers (e.g. Anthropic) attach signatures to thinking blocks
    /// for tamper verification. The signature must be preserved and sent back
    /// when round-tripping thinking blocks in multi-turn conversations.
    Signature(String),
}

impl ContentBlockDelta {
    /// Returns a human-readable label for the variant.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::ToolCall { .. } => "tool_call",
            Self::Reasoning(_) => "reasoning",
            Self::Signature(_) => "signature",
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
        /// Provider-assigned identifier for this response, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Model that produced this response, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
}

/// A boxed stream of [`StreamEvent`]s.
///
/// Returned by streaming methods on [`LlmProvider`](super::provider::LlmProvider),
/// [`Llm`](super::model::Llm), and [`LlmRequestBuilder`](super::builder::LlmRequestBuilder).
pub type LlmStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, super::error::GenerationError>> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_effort_reads_every_named_level_in_any_case() {
        let cases = [
            ("none", ReasoningEffort::None),
            ("Minimal", ReasoningEffort::Minimal),
            ("LOW", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            (" high ", ReasoningEffort::High),
            ("XHigh", ReasoningEffort::XHigh),
            ("max", ReasoningEffort::Max),
        ];
        for (text, expected) in cases {
            assert_eq!(text.parse::<ReasoningEffort>(), Ok(expected), "{text:?}");
        }
    }

    #[test]
    fn reasoning_effort_keeps_unknown_text_as_other_without_changing_case() {
        assert_eq!(
            " Ultra ".parse::<ReasoningEffort>(),
            Ok(ReasoningEffort::Other("Ultra".to_owned()))
        );
    }

    #[test]
    fn reasoning_effort_rejects_empty_text() {
        assert!("".parse::<ReasoningEffort>().is_err());
        assert!(" \t".parse::<ReasoningEffort>().is_err());
    }

    #[test]
    fn reasoning_effort_display_round_trips_through_from_str() {
        let levels = [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
            ReasoningEffort::Other("ultra".to_owned()),
        ];
        for level in levels {
            assert_eq!(level.to_string().parse(), Ok(level.clone()), "{level:?}");
        }
        assert_eq!(ReasoningEffort::XHigh.to_string(), "xhigh");
    }

    #[test]
    fn reasoning_effort_serializes_as_its_name() {
        assert_eq!(
            serde_json::to_value(ReasoningEffort::High).unwrap(),
            serde_json::json!("high")
        );
        assert_eq!(
            serde_json::to_value(ReasoningEffort::Other("Ultra".to_owned())).unwrap(),
            serde_json::json!("Ultra")
        );
        let read: ReasoningEffort = serde_json::from_value(serde_json::json!("HIGH")).unwrap();
        assert_eq!(read, ReasoningEffort::High);
        assert!(serde_json::from_value::<ReasoningEffort>(serde_json::json!("")).is_err());
        assert!(serde_json::from_value::<ReasoningEffort>(serde_json::json!(3)).is_err());
    }

    #[test]
    fn serializing_an_empty_other_effort_is_an_error() {
        for text in ["", " ", " \t\n"] {
            let effort = ReasoningEffort::Other(text.to_owned());
            assert!(serde_json::to_value(&effort).is_err(), "{text:?}");
            let request = LlmRequest {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            assert!(serde_json::to_value(&request).is_err(), "{text:?}");
        }
    }

    #[test]
    fn other_with_a_named_level_text_sends_that_text_but_reads_back_as_the_named_level() {
        let other = ReasoningEffort::Other("high".to_owned());
        assert_ne!(other, ReasoningEffort::High);
        assert_eq!(other.as_str(), ReasoningEffort::High.as_str());
        assert_eq!(
            serde_json::to_value(&other).unwrap(),
            serde_json::to_value(ReasoningEffort::High).unwrap()
        );
        let read: ReasoningEffort =
            serde_json::from_value(serde_json::to_value(&other).unwrap()).unwrap();
        assert_eq!(read, ReasoningEffort::High);
    }

    #[test]
    fn request_without_reasoning_effort_omits_the_field_and_reads_it_back() {
        let request = LlmRequest::default();
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.get("reasoning_effort").is_none(), "{value}");

        let request = LlmRequest {
            reasoning_effort: Some(ReasoningEffort::Max),
            ..Default::default()
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["reasoning_effort"], "max");
        let back: LlmRequest = serde_json::from_value(value).unwrap();
        assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Max));

        // A payload written before the field existed still reads.
        let old: LlmRequest = serde_json::from_value(serde_json::json!({
            "system": null,
            "system_prompt_name": null,
            "system_prompt_version": null,
            "messages": [],
            "tools": null,
            "tool_choice": null,
            "output_schema": null,
            "max_output_tokens": null,
        }))
        .unwrap();
        assert_eq!(old.reasoning_effort, None);
    }

    #[test]
    fn cache_control_default_is_disabled() {
        let cache = CacheControl::default();
        assert!(cache.is_disabled());
        assert!(!cache.is_enabled());
    }

    #[test]
    fn cache_control_prefix_constructor_enables_only_the_prefix() {
        let cache = CacheControl::prefix();
        assert!(cache.prefix);
        assert!(cache.breakpoints.is_empty());
        assert!(cache.is_enabled());
        assert!(!cache.is_disabled());
    }

    #[test]
    fn cache_control_with_breakpoints_sets_indices_and_enables() {
        // Breakpoints alone (no prefix) still count as enabled, and the
        // constructor *replaces* rather than appends.
        let cache = CacheControl::default()
            .with_breakpoints([3, 1])
            .with_breakpoints([0, 2]);
        assert!(!cache.prefix);
        assert_eq!(cache.breakpoints, vec![0, 2]);
        assert!(cache.is_enabled());
    }

    #[test]
    fn cache_control_prefix_and_breakpoints_compose() {
        let cache = CacheControl::prefix().with_breakpoints([4]);
        assert!(cache.prefix);
        assert_eq!(cache.breakpoints, vec![4]);
        assert!(cache.is_enabled());
    }

    #[test]
    fn tool_definition_strict_defaults_true_when_field_absent() {
        // A payload that predates the `strict` field (or otherwise omits it) must
        // deserialize as strict, matching the constructor default. A regression in
        // `default_strict` would silently flip enforcement off for cached/persisted
        // definitions, so pin it here.
        let json = serde_json::json!({
            "name": "legacy",
            "description": "payload without a strict field",
            "parameters": {"type": "object", "properties": {}}
        });
        let def: ToolDefinition = serde_json::from_value(json).unwrap();
        assert!(
            def.strict,
            "missing `strict` must default to true, not false"
        );
    }

    #[test]
    fn tool_definition_strict_round_trips() {
        let def = ToolDefinition::new("t", "d", serde_json::json!({})).with_strict(false);
        let value = serde_json::to_value(&def).unwrap();
        assert_eq!(value["strict"], serde_json::json!(false));
        let back: ToolDefinition = serde_json::from_value(value).unwrap();
        assert!(
            !back.strict,
            "explicit strict = false survives a round-trip"
        );
    }

    /// A call whose `id` and `call_id` differ, like an `OpenAI` function call
    /// (`fc_...` item id, `call_...` call id). A constructor that copies the
    /// wrong field, or only one of them, fails the tests below.
    fn call_with_distinct_ids() -> ToolCall {
        let mut call = ToolCall::new("fc_1", "get_weather", serde_json::json!({"city": "Tokyo"}));
        call.call_id = Some("call_1".to_owned());
        call
    }

    fn into_tool_result(block: UserBlock) -> ToolResult {
        match block {
            UserBlock::ToolResult(result) => result,
            other => panic!("expected a tool result block, got {other:?}"),
        }
    }

    fn into_only_tool_result(message: Message) -> ToolResult {
        match message {
            Message::User { mut content } if content.len() == 1 => {
                into_tool_result(content.remove(0))
            }
            other => panic!("expected a user message with one block, got {other:?}"),
        }
    }

    fn assert_result(result: &ToolResult, expected_status: ToolResultStatus, expected_text: &str) {
        assert_eq!(result.id, "fc_1", "`id` is copied from the call");
        assert_eq!(
            result.call_id.as_deref(),
            Some("call_1"),
            "`call_id` is copied from the call"
        );
        assert_eq!(result.status, expected_status);
        match &result.content {
            ToolResultContent::Text(text) => assert_eq!(text, expected_text),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_result_copies_both_ids_and_marks_success() {
        let call = call_with_distinct_ids();
        let result = into_tool_result(call.result(ToolResultContent::Text("Sunny".into())));
        assert_result(&result, ToolResultStatus::Success, "Sunny");
    }

    #[test]
    fn tool_call_error_copies_both_ids_and_marks_error() {
        let call = call_with_distinct_ids();
        let result = into_tool_result(call.error(ToolResultContent::Text("timed out".into())));
        assert_result(&result, ToolResultStatus::Error, "timed out");
    }

    #[test]
    fn message_tool_result_for_copies_both_ids_and_marks_success() {
        let call = call_with_distinct_ids();
        let message = Message::tool_result_for(&call, ToolResultContent::Text("Sunny".into()));
        assert_result(
            &into_only_tool_result(message),
            ToolResultStatus::Success,
            "Sunny",
        );
    }

    #[test]
    fn message_tool_error_for_copies_both_ids_and_marks_error() {
        let call = call_with_distinct_ids();
        let message = Message::tool_error_for(&call, ToolResultContent::Text("timed out".into()));
        assert_result(
            &into_only_tool_result(message),
            ToolResultStatus::Error,
            "timed out",
        );
    }

    #[test]
    fn tool_call_result_without_call_id_keeps_it_none() {
        // Providers that link by `id` (Anthropic, Bedrock Converse) set no
        // `call_id`; the result must not invent one.
        let call = ToolCall::new("toolu_1", "get_weather", serde_json::json!({}));
        let result = into_tool_result(call.result(ToolResultContent::Text("Sunny".into())));
        assert_eq!(result.id, "toolu_1");
        assert_eq!(result.call_id, None);
    }
}
