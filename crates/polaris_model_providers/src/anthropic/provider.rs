//! Anthropic [`LlmProvider`] implementation.

use super::client::AnthropicClient;
use super::types::{
    self as anthropic_types, ContentBlock, ContentBlockParam, CreateMessageRequest, ImageMediaType,
    ImageSource, MessageParam, OutputFormat, RawStreamEvent, Role, StreamContentBlock, StreamDelta,
    ToolChoiceParam, ToolDef, ToolResultBlock, ToolResultContent,
};
use crate::schema::normalize_schema_for_strict_mode;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures_core::Stream;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, ImageBlock,
    ImageMediaType as PolarisImageMediaType, LlmProvider, LlmRequest, LlmResponse, LlmStream,
    Message, StopReason, StreamEvent, ToolCall, ToolChoice, ToolFunction,
    ToolResultContent as PolarisToolResult, ToolResultStatus, Usage, UserBlock,
};

/// Default maximum tokens for generation requests.
const DEFAULT_MAX_TOKENS: u32 = 4096;

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
            stop_reason: None,
            filtered_indices: Vec::new(),
        }
    }

    fn build_usage(&self) -> Usage {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: Some(input + output),
        }
    }

    fn convert_event(
        &mut self,
        raw: RawStreamEvent,
    ) -> Option<Result<StreamEvent, GenerationError>> {
        match raw {
            RawStreamEvent::MessageStart { message } => {
                self.input_tokens = Some(message.usage.input_tokens);
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
    let messages = request
        .messages
        .iter()
        .map(convert_message)
        .collect::<Result<Vec<_>, _>>()?;

    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|tool| ToolDef {
                name: tool.name.clone(),
                description: Some(tool.description.clone()),
                input_schema: normalize_schema_for_strict_mode(tool.parameters.clone()),
                strict: Some(true),
            })
            .collect()
    });

    let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

    let output_format = request
        .output_schema
        .as_ref()
        .map(|schema| OutputFormat::new(schema.clone()));

    Ok(CreateMessageRequest {
        model: model.to_string(),
        max_tokens: DEFAULT_MAX_TOKENS,
        messages,
        system: request.system.clone(),
        tools,
        tool_choice,
        temperature: None,
        stop_sequences: None,
        output_format,
        stream: None,
    })
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
        }),
        UserBlock::Image(image) => {
            let source = convert_image_to_source(image)?;
            Ok(ContentBlockParam::Image { source })
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
            })
        }
    }
}

fn convert_assistant_block(block: &AssistantBlock) -> Result<ContentBlockParam, GenerationError> {
    match block {
        AssistantBlock::Text(block) => Ok(ContentBlockParam::Text {
            text: block.text.clone(),
        }),
        AssistantBlock::ToolCall(call) => Ok(ContentBlockParam::ToolUse {
            id: call.id.clone(),
            name: call.function.name.clone(),
            input: call.function.arguments.clone(),
        }),
        AssistantBlock::Reasoning(reasoning) => {
            let signature = reasoning.signature.clone().unwrap_or_default();
            Ok(ContentBlockParam::Thinking {
                thinking: reasoning.reasoning.join("\n"),
                signature,
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

    LlmResponse {
        content,
        usage: Usage {
            input_tokens: Some(response.usage.input_tokens),
            output_tokens: Some(response.usage.output_tokens),
            total_tokens: Some(response.usage.input_tokens + response.usage.output_tokens),
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
}
