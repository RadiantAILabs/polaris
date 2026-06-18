//! Anthropic [`LlmProvider`] implementation.

use super::client::AnthropicClient;
use super::types::{
    self as anthropic_types, CacheControlMarker, ContentBlock, ContentBlockParam,
    CreateMessageRequest, ImageMediaType, ImageSource, MessageParam, OutputFormat, RawStreamEvent,
    Role, StreamContentBlock, StreamDelta, SystemBlock, SystemPrompt, ToolChoiceParam, ToolDef,
    ToolResultBlock, ToolResultContent,
};
use crate::schema::normalize_schema_for_strict_mode;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, ImageBlock,
    ImageMediaType as PolarisImageMediaType, LlmProvider, LlmRequest, LlmResponse, LlmStream,
    Message, ModelPricing, StopReason, StreamEvent, ToolCall, ToolChoice, ToolFunction,
    ToolResultContent as PolarisToolResult, ToolResultStatus, Usage, UserBlock,
};

/// Default maximum tokens for generation requests.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Maximum number of tools Anthropic allows to be marked `strict` in a single
/// request. Beyond this the API rejects the request, so we honor each tool's
/// `strict` preference only up to this budget (in request/registration order)
/// and degrade the overflow to non-strict. Strict mode is a best-effort
/// schema-validation optimization, not a correctness requirement.
const MAX_STRICT_TOOLS: usize = 20;

/// Anthropic [`LlmProvider`] implementation.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: AnthropicClient,
}

impl AnthropicProvider {
    /// Creates a new provider.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: AnthropicClient::new(api_key),
        }
    }
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        let anthropic_request = convert_request(model, &request)?;

        let response = self.client.create_message(&anthropic_request).await?;

        Ok(convert_response(response))
    }

    /// # Errors
    ///
    /// Returns [`GenerationError::Auth`] if the API key is not valid UTF-8.
    /// Returns [`GenerationError::Http`] if the HTTP request fails to send.
    /// Returns [`GenerationError::Provider`] if the server responds with a non-2xx
    /// status code.
    /// Returns [`GenerationError::InvalidResponse`] if an SSE frame contains
    /// invalid UTF-8 or unparseable JSON.
    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let anthropic_request = convert_request(model, &request)?;

        let raw_stream = self.client.create_message_stream(anthropic_request).await?;

        Ok(Box::pin(AnthropicStreamAdapter::new(raw_stream)))
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Anthropic public list prices, USD per million base input / output
        // tokens. Cache-tier rates are derived from the base input rate by
        // `ModelPricing::new` (0.1× read, 1.25× write — the 5-minute ephemeral
        // ratios). `starts_with` tolerates dated ids (e.g.
        // `claude-opus-4-7-20260115`). List prices drift and new model ids must
        // be added here; verify against anthropic.com/pricing before relying on
        // the figure for billing.
        let (input_per_million_usd, output_per_million_usd) = if model
            .starts_with("claude-opus-4-5")
            || model.starts_with("claude-opus-4-6")
            || model.starts_with("claude-opus-4-7")
        {
            (5.0, 25.0)
        } else if model.starts_with("claude-opus-4") {
            // Legacy Opus 4 / 4.1.
            (15.0, 75.0)
        } else if model.starts_with("claude-sonnet-4") {
            (3.0, 15.0)
        } else if model.starts_with("claude-haiku-4") {
            (1.0, 5.0)
        } else if model.starts_with("claude-haiku-3-5") {
            (0.8, 4.0)
        } else {
            return None;
        };
        Some(ModelPricing::new(
            input_per_million_usd,
            output_per_million_usd,
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stream Adapter
// ─────────────────────────────────────────────────────────────────────────────

/// Converts a stream of Anthropic [`RawStreamEvent`]s into Polaris [`StreamEvent`]s.
///
/// Tracks input token usage from `message_start` and combines it with output
/// tokens from `message_delta` to produce complete [`Usage`] values.
struct AnthropicStreamAdapter<S> {
    inner: S,
    /// Input tokens from the `message_start` event.
    input_tokens: Option<u64>,
    /// Output tokens from the latest `message_delta` event.
    output_tokens: Option<u64>,
    /// Cache-read tokens from the `message_start` event.
    cache_read_tokens: Option<u64>,
    /// Cache-creation (write) tokens from the `message_start` event.
    cache_creation_tokens: Option<u64>,
    /// Stop reason from the `message_delta` event, held until `message_stop`.
    stop_reason: Option<StopReason>,
    /// Block indices whose `ContentBlockStart` was filtered (e.g. redacted
    /// thinking). Subsequent deltas and stop events for these indices are
    /// also suppressed so the collector never sees an orphaned stop.
    filtered_indices: Vec<u32>,
}

impl<S> AnthropicStreamAdapter<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            stop_reason: None,
            filtered_indices: Vec::new(),
        }
    }

    fn build_usage(&self) -> Usage {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let cache_read = self.cache_read_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_tokens.unwrap_or(0);
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: Some(
                input
                    .saturating_add(output)
                    .saturating_add(cache_read)
                    .saturating_add(cache_creation),
            ),
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
        }
    }

    fn convert_event(
        &mut self,
        raw: RawStreamEvent,
    ) -> Option<Result<StreamEvent, GenerationError>> {
        match raw {
            RawStreamEvent::MessageStart { message } => {
                self.input_tokens = Some(message.usage.input_tokens);
                self.cache_read_tokens = Some(message.usage.cache_read_input_tokens);
                self.cache_creation_tokens = Some(message.usage.cache_creation_input_tokens);
                None
            }
            RawStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let block = match content_block {
                    StreamContentBlock::Text { .. } => ContentBlockStartData::Text,
                    StreamContentBlock::ToolUse { id, name, .. } => {
                        ContentBlockStartData::ToolCall {
                            id,
                            call_id: None,
                            name,
                        }
                    }
                    StreamContentBlock::Thinking { .. } => ContentBlockStartData::Reasoning,
                    StreamContentBlock::RedactedThinking { .. } => {
                        self.filtered_indices.push(index);
                        return None;
                    }
                };
                Some(Ok(StreamEvent::ContentBlockStart { index, block }))
            }
            RawStreamEvent::ContentBlockDelta { index, delta } => {
                if self.filtered_indices.contains(&index) {
                    return None;
                }
                let delta = match delta {
                    StreamDelta::Text { text } => ContentBlockDelta::Text(text),
                    StreamDelta::InputJson { partial_json } => ContentBlockDelta::ToolCall {
                        arguments: partial_json,
                    },
                    StreamDelta::Thinking { thinking } => ContentBlockDelta::Reasoning(thinking),
                    StreamDelta::Signature { signature } => ContentBlockDelta::Signature(signature),
                };
                Some(Ok(StreamEvent::ContentBlockDelta { index, delta }))
            }
            RawStreamEvent::ContentBlockStop { index } => {
                if self.filtered_indices.contains(&index) {
                    return None;
                }
                Some(Ok(StreamEvent::ContentBlockStop { index }))
            }
            RawStreamEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.output_tokens = Some(usage.output_tokens);
                }
                self.stop_reason = Some(convert_stop_reason(delta.stop_reason));
                Some(Ok(StreamEvent::MessageDelta {
                    usage: self.build_usage(),
                }))
            }
            RawStreamEvent::MessageStop => {
                let stop_reason = match self.stop_reason.take() {
                    Some(reason) => reason,
                    None => {
                        tracing::warn!(
                            "message_stop received without a preceding message_delta; \
                             defaulting to EndTurn"
                        );
                        StopReason::EndTurn
                    }
                };
                Some(Ok(StreamEvent::MessageStop {
                    stop_reason,
                    usage: self.build_usage(),
                }))
            }
            RawStreamEvent::Ping => None,
            RawStreamEvent::Error { error } => Some(Err(GenerationError::Provider {
                status: None,
                message: format!("{}: {}", error.error_type, error.message),
                source: None,
            })),
        }
    }
}

impl<S> Stream for AnthropicStreamAdapter<S>
where
    S: Stream<Item = Result<RawStreamEvent, GenerationError>> + Unpin,
{
    type Item = Result<StreamEvent, GenerationError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(raw))) => {
                    if let Some(event) = this.convert_event(raw) {
                        return Poll::Ready(Some(event));
                    }
                    // Event was filtered (ping, message_start, redacted thinking) — poll again.
                }
                Poll::Ready(Some(Err(err))) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request/Response Conversion
// ─────────────────────────────────────────────────────────────────────────────

fn convert_request(
    model: &str,
    request: &LlmRequest,
) -> Result<CreateMessageRequest, GenerationError> {
    let mut messages = request
        .messages
        .iter()
        .map(convert_message)
        .collect::<Result<Vec<_>, _>>()?;

    let mut tools: Option<Vec<ToolDef>> = request.tools.as_ref().map(|tools| {
        // Honor each tool's `strict` preference, but only up to Anthropic's
        // per-request strict-tool budget; tools past the budget (in registration
        // order) degrade to non-strict so the request stays valid. The schema is
        // normalized only when the tool is actually strict — a non-strict tool
        // keeps its full schema rather than having strict-incompatible
        // constructs silently stripped.
        let mut strict_budget = MAX_STRICT_TOOLS;
        tools
            .iter()
            .map(|tool| {
                let strict = tool.strict && strict_budget > 0;
                if strict {
                    strict_budget -= 1;
                } else if tool.strict {
                    // Wanted strict but the per-request budget is spent; degrade so
                    // the request stays valid, and surface it so an operator can see
                    // the tool is being sent without schema enforcement.
                    tracing::debug!(
                        tool = %tool.name,
                        max_strict_tools = MAX_STRICT_TOOLS,
                        "strict-tool budget exhausted; tool sent non-strict"
                    );
                }
                ToolDef {
                    name: tool.name.clone(),
                    description: Some(tool.description.clone()),
                    input_schema: if strict {
                        normalize_schema_for_strict_mode(tool.parameters.clone())
                    } else {
                        tool.parameters.clone()
                    },
                    strict: strict.then_some(true),
                    cache_control: None,
                }
            })
            .collect()
    });

    let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

    let output_format = request
        .output_schema
        .as_ref()
        .map(|schema| OutputFormat::new(schema.clone()));

    let system = apply_cache_control(
        &request.cache,
        request.system.as_deref(),
        tools.as_mut(),
        &mut messages,
    );

    Ok(CreateMessageRequest {
        model: model.to_string(),
        max_tokens: DEFAULT_MAX_TOKENS,
        messages,
        system,
        tools,
        tool_choice,
        temperature: None,
        stop_sequences: None,
        output_format,
        stream: None,
    })
}

/// Anthropic honors at most four `cache_control` breakpoints per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Translate the provider-agnostic [`CacheControl`](polaris_models::llm::CacheControl)
/// into Anthropic `cache_control` markers, returning the `system` field to send.
///
/// The stable prefix (tools → system, in Anthropic's cache order) is covered by a
/// single marker on the system block when a system prompt is present, or on the
/// last tool otherwise — caching everything before it. Each requested message
/// breakpoint marks the last block of that message. Markers are budgeted to
/// [`MAX_CACHE_BREAKPOINTS`]; extras are dropped low-to-high (prefix first, then
/// message breakpoints in order).
fn apply_cache_control(
    cache: &polaris_models::llm::CacheControl,
    system: Option<&str>,
    tools: Option<&mut Vec<ToolDef>>,
    messages: &mut [MessageParam],
) -> Option<SystemPrompt> {
    // Nothing requested: emit the plain system string (if any) and skip all
    // marker work.
    if cache.is_disabled() {
        return system.map(|text| SystemPrompt::Text(text.to_string()));
    }

    let mut budget = MAX_CACHE_BREAKPOINTS;

    // Prefix: one marker covers tools + system. Prefer the system block; fall
    // back to the last tool when there is no system prompt.
    let system = match (cache.prefix, system) {
        (true, Some(text)) if budget > 0 => {
            budget -= 1;
            Some(SystemPrompt::Blocks(vec![SystemBlock::text(
                text.to_string(),
                Some(CacheControlMarker::ephemeral()),
            )]))
        }
        (true, None) if budget > 0 => {
            if let Some(last) = tools.and_then(|tools| tools.last_mut()) {
                last.cache_control = Some(CacheControlMarker::ephemeral());
                budget -= 1;
            }
            None
        }
        (_, Some(text)) => Some(SystemPrompt::Text(text.to_string())),
        (_, None) => None,
    };

    // Message breakpoints: mark the last block of each referenced message.
    for &idx in &cache.breakpoints {
        if budget == 0 {
            break;
        }
        let Some(block) = messages.get_mut(idx).and_then(|msg| msg.content.last_mut()) else {
            // A stale breakpoint (index past the message list, or an empty
            // message with no block to mark) silently produces no marker in
            // release, degrading the cache hit rate with no signal. Trip the
            // assertion in debug to fail fast in tests, and warn in release so
            // the misconfiguration is observable rather than invisible.
            debug_assert!(
                idx < messages.len(),
                "cache breakpoint index {idx} out of range for {} messages; \
                 a context strategy produced a stale breakpoint",
                messages.len()
            );
            tracing::warn!(
                breakpoint = idx,
                message_count = messages.len(),
                "dropping cache breakpoint with no markable block; \
                 a context strategy produced a stale breakpoint"
            );
            continue;
        };
        block.set_cache_control(CacheControlMarker::ephemeral());
        budget -= 1;
    }

    system
}

fn convert_message(message: &Message) -> Result<MessageParam, GenerationError> {
    match message {
        Message::User { content } => {
            let blocks = content
                .iter()
                .map(convert_user_block)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MessageParam {
                role: Role::User,
                content: blocks,
            })
        }
        Message::Assistant { content, .. } => {
            let blocks = content
                .iter()
                .map(convert_assistant_block)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(MessageParam {
                role: Role::Assistant,
                content: blocks,
            })
        }
    }
}

fn convert_image_to_source(image: &ImageBlock) -> Result<ImageSource, GenerationError> {
    let media_type = match image.media_type {
        PolarisImageMediaType::JPEG => ImageMediaType::Jpeg,
        PolarisImageMediaType::PNG => ImageMediaType::Png,
        PolarisImageMediaType::GIF => ImageMediaType::Gif,
        PolarisImageMediaType::WEBP => ImageMediaType::Webp,
        ref other => {
            return Err(GenerationError::UnsupportedContent(format!(
                "Unsupported image media type for Anthropic: {other:?}"
            )));
        }
    };

    match &image.data {
        polaris_models::llm::DocumentSource::Base64(data) => Ok(ImageSource::Base64 {
            media_type,
            data: data.clone(),
        }),
    }
}

fn convert_user_block(block: &UserBlock) -> Result<ContentBlockParam, GenerationError> {
    match block {
        UserBlock::Text(block) => Ok(ContentBlockParam::Text {
            text: block.text.clone(),
            cache_control: None,
        }),
        UserBlock::Image(image) => {
            let source = convert_image_to_source(image)?;
            Ok(ContentBlockParam::Image {
                source,
                cache_control: None,
            })
        }
        UserBlock::Audio(_) => Err(GenerationError::UnsupportedContent(
            "Audio content is not supported by Anthropic".to_string(),
        )),
        UserBlock::Document(_) => Err(GenerationError::UnsupportedContent(
            "Document content is not yet implemented for Anthropic".to_string(),
        )),
        UserBlock::ToolResult(result) => {
            let content = match &result.content {
                PolarisToolResult::Text(text) => Some(ToolResultContent::Text(text.clone())),
                PolarisToolResult::Image(image) => {
                    let source = convert_image_to_source(image)?;
                    Some(ToolResultContent::Blocks(vec![ToolResultBlock::Image {
                        source,
                    }]))
                }
            };
            let is_error = match result.status {
                ToolResultStatus::Success => None,
                ToolResultStatus::Error => Some(true),
            };
            Ok(ContentBlockParam::ToolResult {
                tool_use_id: result.id.clone(),
                content,
                is_error,
                cache_control: None,
            })
        }
    }
}

fn convert_assistant_block(block: &AssistantBlock) -> Result<ContentBlockParam, GenerationError> {
    match block {
        AssistantBlock::Text(block) => Ok(ContentBlockParam::Text {
            text: block.text.clone(),
            cache_control: None,
        }),
        AssistantBlock::ToolCall(call) => Ok(ContentBlockParam::ToolUse {
            id: call.id.clone(),
            name: call.function.name.clone(),
            input: call.function.arguments.clone(),
            cache_control: None,
        }),
        AssistantBlock::Reasoning(reasoning) => {
            let signature = reasoning.signature.clone().unwrap_or_default();
            Ok(ContentBlockParam::Thinking {
                thinking: reasoning.reasoning.join("\n"),
                signature,
                cache_control: None,
            })
        }
    }
}

fn convert_tool_choice(choice: &ToolChoice) -> ToolChoiceParam {
    match choice {
        ToolChoice::Auto => ToolChoiceParam::Auto {
            disable_parallel_tool_use: None,
        },
        ToolChoice::Required => ToolChoiceParam::Any {
            disable_parallel_tool_use: None,
        },
        ToolChoice::Specific(name) => ToolChoiceParam::Tool {
            name: name.clone(),
            disable_parallel_tool_use: None,
        },
        ToolChoice::None => ToolChoiceParam::None,
    }
}

fn convert_response(response: super::types::MessageResponse) -> LlmResponse {
    let stop_reason = convert_stop_reason(response.stop_reason);

    let content = response
        .content
        .into_iter()
        .filter_map(convert_content_block)
        .collect();

    let usage = &response.usage;
    LlmResponse {
        content,
        usage: Usage {
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            total_tokens: Some(
                usage
                    .input_tokens
                    .saturating_add(usage.output_tokens)
                    .saturating_add(usage.cache_read_input_tokens)
                    .saturating_add(usage.cache_creation_input_tokens),
            ),
            cache_read_tokens: Some(usage.cache_read_input_tokens),
            cache_creation_tokens: Some(usage.cache_creation_input_tokens),
        },
        stop_reason,
    }
}

fn convert_stop_reason(stop_reason: anthropic_types::StopReason) -> StopReason {
    match stop_reason {
        anthropic_types::StopReason::MaxTokens => StopReason::MaxOutputTokens,
        anthropic_types::StopReason::StopSequence => StopReason::StopSequence,
        anthropic_types::StopReason::ToolUse => StopReason::ToolUse,
        anthropic_types::StopReason::Refusal => StopReason::ContentFilter,
        anthropic_types::StopReason::PauseTurn => StopReason::Other("pause_turn".to_string()),
        anthropic_types::StopReason::EndTurn => StopReason::EndTurn,
    }
}

fn convert_content_block(block: ContentBlock) -> Option<AssistantBlock> {
    match block {
        ContentBlock::Text { text } => Some(AssistantBlock::Text(polaris_models::llm::TextBlock {
            text,
        })),
        ContentBlock::ToolUse { id, name, input } => Some(AssistantBlock::ToolCall(ToolCall {
            id: id.clone(),
            call_id: None,
            function: ToolFunction {
                name,
                arguments: input,
            },
            signature: None,
            additional_params: None,
        })),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => Some(AssistantBlock::Reasoning(
            polaris_models::llm::ReasoningBlock {
                id: None,
                reasoning: vec![thinking],
                signature: Some(signature),
            },
        )),
        ContentBlock::RedactedThinking { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{
        MessageDeltaPayload, MessageDeltaUsage, MessageStartPayload, StreamErrorInfo, UsageResponse,
    };
    use super::*;

    #[test]
    fn pricing_maps_claude_families_to_list_prices() {
        let provider = AnthropicProvider::new("test-key");
        // Opus 4.5+ is the $5 / $25 tier.
        assert_eq!(
            provider.pricing("claude-opus-4-7"),
            Some(ModelPricing::new(5.0, 25.0))
        );
        // Legacy Opus 4.1 stays on the $15 / $75 tier.
        assert_eq!(
            provider.pricing("claude-opus-4-1"),
            Some(ModelPricing::new(15.0, 75.0))
        );
        assert_eq!(
            provider.pricing("claude-sonnet-4-6"),
            Some(ModelPricing::new(3.0, 15.0))
        );
        // Dated ids still match the family.
        assert_eq!(
            provider.pricing("claude-haiku-4-5-20251001"),
            Some(ModelPricing::new(1.0, 5.0))
        );
        assert_eq!(provider.pricing("some-unknown-model"), None);
    }

    #[test]
    fn converts_text_stream() {
        let mut adapter = AnthropicStreamAdapter::new(());

        let events = vec![
            RawStreamEvent::MessageStart {
                message: MessageStartPayload {
                    usage: UsageResponse {
                        input_tokens: 10,
                        output_tokens: 0,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    },
                },
            },
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: StreamContentBlock::Text {
                    text: String::new(),
                },
            },
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::Text {
                    text: "Hello".to_string(),
                },
            },
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::Text {
                    text: " world".to_string(),
                },
            },
            RawStreamEvent::ContentBlockStop { index: 0 },
            RawStreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: anthropic_types::StopReason::EndTurn,
                },
                usage: Some(MessageDeltaUsage { output_tokens: 5 }),
            },
            RawStreamEvent::MessageStop,
        ];

        let converted: Vec<_> = events
            .into_iter()
            .filter_map(|raw| adapter.convert_event(raw))
            .collect();

        // 7 raw events: MessageStart(None), ContentBlockStart, delta, delta,
        // ContentBlockStop, MessageDelta, MessageStop = 6 converted events.
        assert_eq!(converted.len(), 6);

        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text
            }
        ));
        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == "Hello"
        ));
        assert!(matches!(
            converted[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == " world"
        ));
        assert!(matches!(
            converted[3].as_ref().unwrap(),
            StreamEvent::ContentBlockStop { index: 0 }
        ));
        assert!(matches!(
            converted[4].as_ref().unwrap(),
            StreamEvent::MessageDelta { .. }
        ));
        assert!(matches!(
            converted[5].as_ref().unwrap(),
            StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    #[test]
    fn converts_tool_use_stream() {
        let mut adapter = AnthropicStreamAdapter::new(());

        let events = vec![
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: StreamContentBlock::ToolUse {
                    id: "tool_1".to_string(),
                    name: "get_weather".to_string(),
                    input: serde_json::Value::Object(serde_json::Map::new()),
                },
            },
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::InputJson {
                    partial_json: r#"{"city""#.to_string(),
                },
            },
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::InputJson {
                    partial_json: r#":"London"}"#.to_string(),
                },
            },
            RawStreamEvent::ContentBlockStop { index: 0 },
        ];

        let converted: Vec<_> = events
            .into_iter()
            .filter_map(|raw| adapter.convert_event(raw))
            .collect();

        assert_eq!(converted.len(), 4);

        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall { id, name, .. }
            } if id == "tool_1" && name == "get_weather"
        ));

        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall { arguments }
            } if arguments == r#"{"city""#
        ));
    }

    #[test]
    fn converts_thinking_stream() {
        let mut adapter = AnthropicStreamAdapter::new(());

        let events = vec![
            RawStreamEvent::ContentBlockStart {
                index: 0,
                content_block: StreamContentBlock::Thinking {
                    thinking: String::new(),
                },
            },
            RawStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::Thinking {
                    thinking: "Let me think...".to_string(),
                },
            },
            RawStreamEvent::ContentBlockStop { index: 0 },
        ];

        let converted: Vec<_> = events
            .into_iter()
            .filter_map(|raw| adapter.convert_event(raw))
            .collect();

        assert_eq!(converted.len(), 3);
        assert!(matches!(
            converted[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                block: ContentBlockStartData::Reasoning,
                ..
            }
        ));
        assert!(matches!(
            converted[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Reasoning(t), .. } if t == "Let me think..."
        ));
    }

    #[test]
    fn usage_tracks_across_events() {
        let mut adapter = AnthropicStreamAdapter::new(());

        adapter.convert_event(RawStreamEvent::MessageStart {
            message: MessageStartPayload {
                usage: UsageResponse {
                    input_tokens: 25,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
            },
        });

        let msg_delta = adapter.convert_event(RawStreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: anthropic_types::StopReason::EndTurn,
            },
            usage: Some(MessageDeltaUsage { output_tokens: 15 }),
        });

        let delta_event = msg_delta.unwrap().unwrap();
        assert!(matches!(
            &delta_event,
            StreamEvent::MessageDelta { usage } if usage.input_tokens == Some(25) && usage.output_tokens == Some(15)
        ));

        let msg_stop = adapter
            .convert_event(RawStreamEvent::MessageStop)
            .unwrap()
            .unwrap();
        assert!(matches!(
            &msg_stop,
            StreamEvent::MessageStop { stop_reason: StopReason::EndTurn, usage }
                if usage.input_tokens == Some(25) && usage.output_tokens == Some(15) && usage.total_tokens == Some(40)
        ));
    }

    #[test]
    fn error_event_maps_to_generation_error() {
        let mut adapter = AnthropicStreamAdapter::new(());

        let result = adapter.convert_event(RawStreamEvent::Error {
            error: StreamErrorInfo {
                error_type: "overloaded_error".to_string(),
                message: "Overloaded".to_string(),
            },
        });

        assert!(result.unwrap().is_err());
    }

    #[test]
    fn ping_is_filtered() {
        let mut adapter = AnthropicStreamAdapter::new(());
        assert!(adapter.convert_event(RawStreamEvent::Ping).is_none());
    }

    #[test]
    fn redacted_thinking_is_filtered() {
        let mut adapter = AnthropicStreamAdapter::new(());
        let result = adapter.convert_event(RawStreamEvent::ContentBlockStart {
            index: 0,
            content_block: StreamContentBlock::RedactedThinking {
                data: "redacted".to_string(),
            },
        });
        assert!(result.is_none());
    }

    #[test]
    fn redacted_thinking_filters_entire_block() {
        let mut adapter = AnthropicStreamAdapter::new(());

        // ContentBlockStart for redacted thinking should be filtered.
        let start = adapter.convert_event(RawStreamEvent::ContentBlockStart {
            index: 1,
            content_block: StreamContentBlock::RedactedThinking {
                data: "redacted".to_string(),
            },
        });
        assert!(start.is_none(), "redacted start should be filtered");

        // ContentBlockStop for the same index should also be filtered.
        let stop = adapter.convert_event(RawStreamEvent::ContentBlockStop { index: 1 });
        assert!(
            stop.is_none(),
            "stop for redacted block should also be filtered"
        );

        // A stop for a non-filtered index should still pass through.
        let other_stop = adapter.convert_event(RawStreamEvent::ContentBlockStop { index: 0 });
        assert!(
            other_stop.is_some(),
            "stop for other index should pass through"
        );
    }

    #[test]
    fn signature_delta_is_preserved() {
        let mut adapter = AnthropicStreamAdapter::new(());
        let result = adapter.convert_event(RawStreamEvent::ContentBlockDelta {
            index: 0,
            delta: StreamDelta::Signature {
                signature: "EqQBCgIYAhIM...".to_string(),
            },
        });
        let event = result.expect("signature should not be filtered").unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Signature(s)
            } if s == "EqQBCgIYAhIM..."
        ));
    }

    // ── Prompt caching: request-side cache_control emission ──

    use polaris_models::llm::{CacheControl, LlmRequest, ToolDefinition};

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            format!("{name} tool"),
            serde_json::json!({"type": "object", "properties": {}}),
        )
    }

    /// The JSON the wire request serializes to.
    fn request_json(request: &LlmRequest) -> serde_json::Value {
        serde_json::to_value(convert_request("claude-sonnet-4-6", request).unwrap()).unwrap()
    }

    #[test]
    fn cache_prefix_marks_the_system_block() {
        let request = LlmRequest {
            system: Some("You are helpful".to_string()),
            messages: vec![Message::user("hi")],
            tools: Some(vec![tool_def("search")]),
            cache: CacheControl::prefix(),
            ..Default::default()
        };
        let json = request_json(&request);
        // System is emitted as a block array carrying the ephemeral marker.
        let system = &json["system"];
        assert!(
            system.is_array(),
            "system should be a block array: {system}"
        );
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        // One marker covers tools+system, so the last tool is NOT separately marked.
        assert!(json["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_prefix_without_system_marks_last_tool() {
        let request = LlmRequest {
            system: None,
            messages: vec![Message::user("hi")],
            tools: Some(vec![tool_def("a"), tool_def("b")]),
            cache: CacheControl::prefix(),
            ..Default::default()
        };
        let json = request_json(&request);
        assert!(json.get("system").is_none() || json["system"].is_null());
        assert!(json["tools"][0].get("cache_control").is_none());
        assert_eq!(json["tools"][1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn tools_within_cap_are_marked_strict() {
        let request = LlmRequest {
            messages: vec![Message::user("hi")],
            tools: Some(
                (0..MAX_STRICT_TOOLS)
                    .map(|i| tool_def(&format!("t{i}")))
                    .collect(),
            ),
            ..Default::default()
        };
        let json = request_json(&request);
        for tool in json["tools"].as_array().unwrap() {
            assert_eq!(
                tool["strict"], true,
                "tool should be strict at the cap: {tool}"
            );
        }
    }

    #[test]
    fn strict_budget_degrades_overflow_in_order() {
        // One past the cap: the first MAX_STRICT_TOOLS keep strict, the overflow
        // degrades — so the request stays under Anthropic's strict-tool limit
        // without dropping strictness from the whole batch.
        let request = LlmRequest {
            messages: vec![Message::user("hi")],
            tools: Some(
                (0..=MAX_STRICT_TOOLS)
                    .map(|i| tool_def(&format!("t{i}")))
                    .collect(),
            ),
            ..Default::default()
        };
        let json = request_json(&request);
        let tools = json["tools"].as_array().unwrap();
        let strict_count = tools.iter().filter(|t| t["strict"] == true).count();
        assert_eq!(
            strict_count, MAX_STRICT_TOOLS,
            "exactly the budget stays strict"
        );
        assert!(
            tools.last().unwrap().get("strict").is_none(),
            "overflow tool should be non-strict: {}",
            tools.last().unwrap()
        );
    }

    #[test]
    fn per_tool_strict_opt_out_is_honored_and_frees_budget() {
        // A tool that opts out of strict is sent non-strict and does not consume
        // a strict-budget slot, so a later tool still fits under the cap.
        let mut tools: Vec<_> = (0..MAX_STRICT_TOOLS)
            .map(|i| tool_def(&format!("t{i}")))
            .collect();
        tools[0] = tools[0].clone().with_strict(false);
        tools.push(tool_def("extra"));
        let request = LlmRequest {
            messages: vec![Message::user("hi")],
            tools: Some(tools),
            ..Default::default()
        };
        let json = request_json(&request);
        let tools = json["tools"].as_array().unwrap();
        assert!(
            tools[0].get("strict").is_none(),
            "opted-out tool stays non-strict: {}",
            tools[0]
        );
        // Budget freed by t0's opt-out is taken by the extra tool at the tail.
        assert_eq!(tools.last().unwrap()["strict"], true);
        let strict_count = tools.iter().filter(|t| t["strict"] == true).count();
        assert_eq!(strict_count, MAX_STRICT_TOOLS);
    }

    #[test]
    fn cache_breakpoints_mark_message_blocks() {
        let request = LlmRequest {
            system: Some("sys".to_string()),
            messages: vec![
                Message::user("first"),
                Message::assistant("second"),
                Message::user("third"),
            ],
            cache: CacheControl::default().with_breakpoints([0, 2]),
            ..Default::default()
        };
        let json = request_json(&request);
        let msgs = &json["messages"];
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(msgs[1]["content"][0].get("cache_control").is_none());
        assert_eq!(msgs[2]["content"][0]["cache_control"]["type"], "ephemeral");
        // No prefix requested → plain system string.
        assert!(json["system"].is_string());
    }

    #[test]
    fn cache_markers_are_budget_capped_at_four() {
        // Prefix (1) + five message breakpoints; only three messages can also be
        // marked before the four-marker budget is exhausted.
        let request = LlmRequest {
            system: Some("sys".to_string()),
            messages: (0..5).map(|i| Message::user(format!("m{i}"))).collect(),
            cache: CacheControl::prefix().with_breakpoints([0, 1, 2, 3, 4]),
            ..Default::default()
        };
        let json = request_json(&request);
        let marked: Vec<usize> = (0..5)
            .filter(|&i| {
                json["messages"][i]["content"][0]
                    .get("cache_control")
                    .is_some()
            })
            .collect();
        // Prefix takes one marker; the remaining three go to the *lowest*
        // breakpoint indices in order (0, 1, 2), and 3 & 4 are dropped — locking
        // the documented low-to-high drop order, not just the count.
        assert_eq!(
            marked,
            vec![0, 1, 2],
            "prefix + 3 lowest message markers = 4 total; 3 & 4 dropped"
        );
    }

    #[test]
    fn no_cache_control_emits_plain_string_system() {
        let request = LlmRequest {
            system: Some("sys".to_string()),
            messages: vec![Message::user("hi")],
            ..Default::default()
        };
        let json = request_json(&request);
        assert!(json["system"].is_string());
        assert!(
            json["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn cache_prefix_without_system_or_tools_is_a_noop() {
        // Prefix requested but nothing to anchor it on (no system, no tools, or
        // an empty tool list): no marker is emitted and no message is touched.
        for tools in [None, Some(vec![])] {
            let request = LlmRequest {
                system: None,
                messages: vec![Message::user("hi")],
                tools,
                cache: CacheControl::prefix(),
                ..Default::default()
            };
            let json = request_json(&request);
            assert!(json.get("system").is_none() || json["system"].is_null());
            assert!(
                json["messages"][0]["content"][0]
                    .get("cache_control")
                    .is_none()
            );
        }
    }

    // ── Prompt caching: cache-usage parsing ──

    #[test]
    fn nonstream_usage_parses_cache_tokens() {
        // Mirror of `stream_usage_parses_cache_tokens` for the non-stream
        // `convert_response` path: cache tokens are surfaced and the total folds
        // input + output + cache_read + cache_creation.
        let response = super::super::types::MessageResponse {
            id: "msg_1".to_string(),
            message_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![],
            model: "claude-sonnet-4-6".to_string(),
            stop_reason: anthropic_types::StopReason::EndTurn,
            stop_sequence: None,
            usage: UsageResponse {
                input_tokens: 100,
                output_tokens: 10,
                cache_creation_input_tokens: 20,
                cache_read_input_tokens: 500,
            },
        };
        let converted = convert_response(response);
        assert_eq!(converted.usage.input_tokens, Some(100));
        assert_eq!(converted.usage.output_tokens, Some(10));
        assert_eq!(converted.usage.cache_read_tokens, Some(500));
        assert_eq!(converted.usage.cache_creation_tokens, Some(20));
        assert_eq!(converted.usage.total_tokens, Some(100 + 10 + 500 + 20));
    }

    #[test]
    fn stream_usage_parses_cache_tokens() {
        let mut adapter = AnthropicStreamAdapter::new(());
        adapter.convert_event(RawStreamEvent::MessageStart {
            message: MessageStartPayload {
                usage: UsageResponse {
                    input_tokens: 100,
                    output_tokens: 0,
                    cache_creation_input_tokens: 20,
                    cache_read_input_tokens: 500,
                },
            },
        });
        let stop = adapter
            .convert_event(RawStreamEvent::MessageDelta {
                delta: MessageDeltaPayload {
                    stop_reason: anthropic_types::StopReason::EndTurn,
                },
                usage: Some(MessageDeltaUsage { output_tokens: 10 }),
            })
            .unwrap()
            .unwrap();
        match stop {
            StreamEvent::MessageDelta { usage } => {
                assert_eq!(usage.input_tokens, Some(100));
                assert_eq!(usage.cache_read_tokens, Some(500));
                assert_eq!(usage.cache_creation_tokens, Some(20));
                // total = input + output + cache_read + cache_creation.
                assert_eq!(usage.total_tokens, Some(100 + 10 + 500 + 20));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn cached_input_is_billed_at_the_discount_tier() {
        // Sonnet: $3/M input, derived 0.1× read ($0.30/M) and 1.25× write ($3.75/M).
        let rate = AnthropicProvider::new("k")
            .pricing("claude-sonnet-4-6")
            .unwrap();
        // 1M cache-read tokens cost a tenth of 1M full-price input tokens.
        let full = rate.cost(1_000_000, 0);
        let cached = rate.cost_with_cache(0, 0, 1_000_000, 0);
        assert!((full - 3.0).abs() < 1e-9, "full input = $3");
        assert!((cached - 0.30).abs() < 1e-9, "cache read = $0.30");
    }

    #[test]
    fn cache_breakpoint_marks_tool_use_and_tool_result_blocks() {
        // A breakpoint can land on a message whose last block is a tool_use
        // (assistant) or tool_result (user), not just plain text — the marker
        // must attach to those `ContentBlockParam` arms too, not only `Text`.
        let request = LlmRequest {
            messages: vec![
                Message::assistant_tool_call(ToolCall::new(
                    "call_1",
                    "search",
                    serde_json::json!({ "q": "x" }),
                )),
                Message::tool_result("call_1", PolarisToolResult::Text("done".to_string())),
            ],
            cache: CacheControl::default().with_breakpoints([0, 1]),
            ..Default::default()
        };
        let json = request_json(&request);
        let msgs = &json["messages"];
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(
            msgs[0]["content"][0]["cache_control"]["type"], "ephemeral",
            "tool_use block must carry the cache marker: {}",
            msgs[0]
        );
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(
            msgs[1]["content"][0]["cache_control"]["type"], "ephemeral",
            "tool_result block must carry the cache marker: {}",
            msgs[1]
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "out of range")]
    fn out_of_range_breakpoint_trips_debug_assert() {
        // A stale breakpoint index (past the message list) is a context-strategy
        // bug, caught loudly in debug builds. In release the `debug_assert!` is
        // compiled out and `messages.get_mut(idx)` skips it (logging a
        // `tracing::warn!`), leaving other markers intact — so this guard never
        // panics in production.
        let request = LlmRequest {
            messages: vec![Message::user("only one")],
            cache: CacheControl::default().with_breakpoints([5]),
            ..Default::default()
        };
        let _ = request_json(&request);
    }

    #[test]
    fn total_tokens_saturates_instead_of_overflowing() {
        // A response whose token tiers sum past u64::MAX must saturate, not
        // panic on debug-overflow — guards the `saturating_add` folding in
        // `convert_response`.
        let response = super::super::types::MessageResponse {
            id: "msg_overflow".to_string(),
            message_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![],
            model: "claude-sonnet-4-6".to_string(),
            stop_reason: anthropic_types::StopReason::EndTurn,
            stop_sequence: None,
            usage: UsageResponse {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cache_creation_input_tokens: u64::MAX,
                cache_read_input_tokens: u64::MAX,
            },
        };
        let converted = convert_response(response);
        assert_eq!(
            converted.usage.total_tokens,
            Some(u64::MAX),
            "summed token tiers must saturate at u64::MAX, not wrap or panic"
        );
    }
}
