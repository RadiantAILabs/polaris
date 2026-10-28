//! Bedrock to Polaris response conversions.

use super::provider::map_sdk_error;
use super::types::document_to_json;
use aws_sdk_bedrockruntime::operation::converse::ConverseOutput;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamOutput as ConverseStreamResponse;
use aws_sdk_bedrockruntime::types as bedrock;
use polaris_models::llm::{
    self as polaris_llm, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmResponse,
    LlmStream, StopReason, StreamEvent, Usage,
};
use std::collections::{HashMap, VecDeque};

/// Converts a Bedrock converse response to a Polaris generation response.
pub fn convert_response(response: ConverseOutput) -> Result<LlmResponse, GenerationError> {
    let content = match response.output {
        Some(bedrock::ConverseOutput::Message(msg)) => msg
            .content
            .into_iter()
            .map(convert_content_block)
            .collect::<Result<Vec<_>, _>>()?,
        Some(unexpected) => {
            return Err(GenerationError::InvalidResponse(format!(
                "unexpected output type {unexpected:?} from Bedrock"
            )));
        }
        None => Vec::new(),
    };

    let usage = convert_usage(response.usage);
    let stop_reason = convert_stop_reason(response.stop_reason)?;

    Ok(LlmResponse {
        content,
        usage,
        stop_reason,
        id: None,
        model: None,
    })
}

fn convert_stop_reason(stop_reason: bedrock::StopReason) -> Result<StopReason, GenerationError> {
    match stop_reason {
        bedrock::StopReason::EndTurn => Ok(StopReason::EndTurn),
        bedrock::StopReason::StopSequence => Ok(StopReason::StopSequence),
        bedrock::StopReason::MaxTokens | bedrock::StopReason::ModelContextWindowExceeded => {
            Ok(StopReason::MaxOutputTokens)
        }
        bedrock::StopReason::ToolUse => Ok(StopReason::ToolUse),
        bedrock::StopReason::ContentFiltered | bedrock::StopReason::GuardrailIntervened => {
            Ok(StopReason::ContentFilter)
        }
        other => Ok(StopReason::Other(other.as_str().to_string())),
    }
}

/// Converts a Bedrock content block to a Polaris assistant block.
fn convert_content_block(
    block: bedrock::ContentBlock,
) -> Result<polaris_llm::AssistantBlock, GenerationError> {
    match block {
        bedrock::ContentBlock::Text(text) => {
            Ok(polaris_llm::AssistantBlock::Text(polaris_llm::TextBlock {
                text,
            }))
        }
        bedrock::ContentBlock::ToolUse(tool_use) => Ok(convert_tool_use(tool_use)),
        bedrock::ContentBlock::ReasoningContent(reasoning) => convert_reasoning(reasoning),
        other => Err(GenerationError::InvalidResponse(format!(
            "unsupported Bedrock content block type: {other:?}"
        ))),
    }
}

/// Converts a Bedrock tool use block to a Polaris tool call.
fn convert_tool_use(tool_use: bedrock::ToolUseBlock) -> polaris_llm::AssistantBlock {
    polaris_llm::AssistantBlock::ToolCall(polaris_llm::ToolCall {
        id: tool_use.tool_use_id,
        call_id: None,
        function: polaris_llm::ToolFunction {
            name: tool_use.name,
            arguments: document_to_json(&tool_use.input),
        },
        signature: None,
        additional_params: None,
    })
}

/// Converts a Bedrock reasoning content block to a Polaris reasoning block.
fn convert_reasoning(
    reasoning: bedrock::ReasoningContentBlock,
) -> Result<polaris_llm::AssistantBlock, GenerationError> {
    match reasoning {
        bedrock::ReasoningContentBlock::ReasoningText(block) => Ok(
            polaris_llm::AssistantBlock::Reasoning(polaris_llm::ReasoningBlock {
                id: None,
                reasoning: vec![block.text],
                signature: block.signature,
            }),
        ),
        bedrock::ReasoningContentBlock::RedactedContent(_) => {
            // Redacted reasoning is encrypted by the provider for safety.
            // We represent it as an empty reasoning block since the content
            // is not accessible.
            Ok(polaris_llm::AssistantBlock::Reasoning(
                polaris_llm::ReasoningBlock {
                    id: None,
                    reasoning: Vec::new(),
                    signature: None,
                },
            ))
        }
        _ => Err(GenerationError::InvalidResponse(
            "unknown Bedrock reasoning content variant".to_string(),
        )),
    }
}

/// Converts Bedrock token usage to Polaris usage.
///
/// Bedrock reports cache tokens upstream (`cacheReadInputTokens` /
/// `cacheWriteInputTokens`) and its `inputTokens` already excludes them, so the
/// cache counts are surfaced directly as `cache_read_tokens` /
/// `cache_creation_tokens` — otherwise cached input is silently under-counted in
/// cost estimates. Bedrock does not report reasoning tokens.
fn convert_usage(usage: Option<bedrock::TokenUsage>) -> Usage {
    usage.map_or_else(Usage::default, |u| Usage {
        input_tokens: Some(u.input_tokens as u64),
        output_tokens: Some(u.output_tokens as u64),
        total_tokens: Some(u.total_tokens as u64),
        cache_read_tokens: u.cache_read_input_tokens.map(|t| t as u64),
        cache_creation_tokens: u.cache_write_input_tokens.map(|t| t as u64),
        reasoning_output_tokens: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming
// ─────────────────────────────────────────────────────────────────────────────

/// Converts a Bedrock `ConverseStream` response into a Polaris [`LlmStream`].
#[must_use]
pub fn convert_stream(output: ConverseStreamResponse) -> LlmStream {
    let state = StreamState {
        output,
        done: false,
        acc: StreamAccumulator::new(),
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(item) = state.acc.pending.pop_front() {
                    return Some((item, state));
                }
                if state.done {
                    return None;
                }
                match state.output.stream.recv().await {
                    Ok(Some(event)) => state.acc.observe(event),
                    Ok(None) => {
                        state.done = true;
                        state.acc.flush_message_stop();
                    }
                    Err(err) => {
                        state.done = true;
                        state.acc.pending.push_back(Err(map_sdk_error(err)));
                    }
                }
            }
        },
    ))
}

/// The mechanical bridge from an async `recv()`-based event source to a
/// [`futures_core::Stream`]: owns the [`ConverseStreamResponse`] (and, through
/// its public `stream` field, the event receiver) so
/// `futures_util::stream::unfold` can drive it without a self-referential
/// borrow — each step moves `self` into the async block, calls
/// `recv().await`, and hands ownership back out. All the actual event ->
/// [`StreamEvent`] conversion logic lives in [`StreamAccumulator`], which
/// needs no receiver and is directly unit-testable.
struct StreamState {
    output: ConverseStreamResponse,
    done: bool,
    acc: StreamAccumulator,
}

/// What kind of content block is open at a given stream index.
///
/// Bedrock's `ContentBlockStart` carries no variant for text or reasoning
/// blocks (only `Image`/`ToolResult`/`ToolUse`), so those two are classified
/// lazily from the first delta observed at that index instead of from the
/// start event.
enum OpenBlock {
    Text,
    Reasoning,
    ToolCall,
    /// The block's reasoning was redacted by the provider for safety. The
    /// whole block is dropped — no `ContentBlockStart`/`Delta`/`Stop` reaches
    /// the caller — mirroring the Anthropic stream adapter's handling of
    /// `redacted_thinking`.
    Filtered,
}

/// Accumulates a Bedrock `ConverseStream` into Polaris [`StreamEvent`]s.
///
/// Pure conversion state — no event receiver, so it can be driven directly in
/// tests with hand-built [`bedrock::ConverseStreamOutput`] events instead of
/// a real Bedrock connection.
struct StreamAccumulator {
    /// Events already converted from the last observed Bedrock event, not
    /// yet handed to the caller — one Bedrock event can produce more than one
    /// Polaris [`StreamEvent`] (a deferred `ContentBlockStart` plus the delta
    /// that revealed its kind).
    pending: VecDeque<Result<StreamEvent, GenerationError>>,
    open_blocks: HashMap<i32, OpenBlock>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    /// Held from `MessageStop` until `Metadata` (which carries usage) arrives
    /// — the reverse of Anthropic's ordering, where usage arrives first.
    stop_reason: Option<StopReason>,
    /// Guards [`flush_message_stop`](Self::flush_message_stop) so a stream
    /// that ends right after `Metadata` doesn't emit a second, empty
    /// `MessageStop`.
    flushed_stop: bool,
}

impl StreamAccumulator {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            open_blocks: HashMap::new(),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            stop_reason: None,
            flushed_stop: false,
        }
    }

    fn observe(&mut self, event: bedrock::ConverseStreamOutput) {
        match event {
            bedrock::ConverseStreamOutput::ContentBlockStart(start) => {
                self.observe_block_start(start);
            }
            bedrock::ConverseStreamOutput::ContentBlockDelta(delta) => {
                self.observe_block_delta(delta);
            }
            bedrock::ConverseStreamOutput::ContentBlockStop(stop) => {
                self.observe_block_stop(stop);
            }
            bedrock::ConverseStreamOutput::MessageStop(stop) => {
                self.stop_reason =
                    Some(convert_stop_reason(stop.stop_reason).unwrap_or(StopReason::EndTurn));
            }
            bedrock::ConverseStreamOutput::Metadata(metadata) => {
                if let Some(usage) = metadata.usage {
                    self.input_tokens = Some(usage.input_tokens as u64);
                    self.output_tokens = Some(usage.output_tokens as u64);
                    self.cache_read_tokens = usage.cache_read_input_tokens.map(|t| t as u64);
                    self.cache_creation_tokens = usage.cache_write_input_tokens.map(|t| t as u64);
                }
                self.flush_message_stop();
            }
            // MessageStart carries only the role (always "assistant"); other
            // variants (Image/ToolResult content we don't model, and any
            // future non-exhaustive addition) carry nothing Polaris
            // represents.
            _ => {}
        }
    }

    fn observe_block_start(&mut self, event: bedrock::ContentBlockStartEvent) {
        let index = event.content_block_index;
        if let Some(bedrock::ContentBlockStart::ToolUse(tool_use)) = event.start {
            // Tool-call blocks are classified here (not lazily from the
            // delta), since Bedrock's start event already names the tool.
            self.open_blocks.insert(index, OpenBlock::ToolCall);
            self.pending.push_back(Ok(StreamEvent::ContentBlockStart {
                index: index as u32,
                block: ContentBlockStartData::ToolCall {
                    id: tool_use.tool_use_id,
                    call_id: None,
                    name: tool_use.name,
                },
            }));
        }
        // Image/ToolResult starts (and any future non-exhaustive variant)
        // have no Polaris content-block equivalent; text/reasoning starts
        // carry no `start` variant at all and are classified lazily below.
    }

    /// Emits the deferred `ContentBlockStart` for `index` the first time it
    /// is observed (from the first delta, since Bedrock sends no start event
    /// for text/reasoning blocks); a no-op on every later delta for the same
    /// index.
    fn classify(&mut self, index: i32, kind: OpenBlock) {
        if self.open_blocks.contains_key(&index) {
            return;
        }
        let block = match kind {
            OpenBlock::Text => ContentBlockStartData::Text,
            OpenBlock::Reasoning => ContentBlockStartData::Reasoning,
            OpenBlock::Filtered => {
                self.open_blocks.insert(index, OpenBlock::Filtered);
                return;
            }
            OpenBlock::ToolCall => {
                unreachable!(
                    "classify() is only called with Text/Reasoning/Filtered; \
                     tool-call blocks are classified directly in observe_block_start"
                )
            }
        };
        self.open_blocks.insert(index, kind);
        self.pending.push_back(Ok(StreamEvent::ContentBlockStart {
            index: index as u32,
            block,
        }));
    }

    fn observe_block_delta(&mut self, event: bedrock::ContentBlockDeltaEvent) {
        let index = event.content_block_index;
        if matches!(self.open_blocks.get(&index), Some(OpenBlock::Filtered)) {
            return;
        }
        let Some(delta) = event.delta else { return };
        match delta {
            bedrock::ContentBlockDelta::Text(text) => {
                self.classify(index, OpenBlock::Text);
                self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index: index as u32,
                    delta: ContentBlockDelta::Text(text),
                }));
            }
            bedrock::ContentBlockDelta::ToolUse(tool_use_delta) => {
                self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index: index as u32,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: tool_use_delta.input,
                    },
                }));
            }
            bedrock::ContentBlockDelta::ReasoningContent(reasoning_delta) => {
                self.observe_reasoning_delta(index, reasoning_delta);
            }
            // Citation/image deltas have no Polaris equivalent; skip.
            _ => {}
        }
    }

    fn observe_reasoning_delta(&mut self, index: i32, delta: bedrock::ReasoningContentBlockDelta) {
        match delta {
            bedrock::ReasoningContentBlockDelta::Text(text) => {
                self.classify(index, OpenBlock::Reasoning);
                self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index: index as u32,
                    delta: ContentBlockDelta::Reasoning(text),
                }));
            }
            bedrock::ReasoningContentBlockDelta::Signature(signature) => {
                self.classify(index, OpenBlock::Reasoning);
                self.pending.push_back(Ok(StreamEvent::ContentBlockDelta {
                    index: index as u32,
                    delta: ContentBlockDelta::Signature(signature),
                }));
            }
            bedrock::ReasoningContentBlockDelta::RedactedContent(_) => {
                self.classify(index, OpenBlock::Filtered);
            }
            _ => {}
        }
    }

    fn observe_block_stop(&mut self, event: bedrock::ContentBlockStopEvent) {
        let index = event.content_block_index;
        if matches!(self.open_blocks.get(&index), Some(OpenBlock::Filtered)) {
            return;
        }
        self.pending.push_back(Ok(StreamEvent::ContentBlockStop {
            index: index as u32,
        }));
    }

    fn flush_message_stop(&mut self) {
        if self.flushed_stop {
            return;
        }
        self.flushed_stop = true;
        let stop_reason = self.stop_reason.take().unwrap_or_else(|| {
            tracing::warn!(
                "converse_stream ended without a message_stop event; defaulting to EndTurn"
            );
            StopReason::EndTurn
        });
        self.pending.push_back(Ok(StreamEvent::MessageStop {
            stop_reason,
            usage: self.build_usage(),
            id: None,
            model: None,
        }));
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
            reasoning_output_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_reason_cases() -> Vec<(bedrock::StopReason, StopReason)> {
        vec![
            (bedrock::StopReason::EndTurn, StopReason::EndTurn),
            (bedrock::StopReason::StopSequence, StopReason::StopSequence),
            (bedrock::StopReason::MaxTokens, StopReason::MaxOutputTokens),
            (
                bedrock::StopReason::ModelContextWindowExceeded,
                StopReason::MaxOutputTokens,
            ),
            (bedrock::StopReason::ToolUse, StopReason::ToolUse),
            (
                bedrock::StopReason::ContentFiltered,
                StopReason::ContentFilter,
            ),
            (
                bedrock::StopReason::GuardrailIntervened,
                StopReason::ContentFilter,
            ),
            (
                bedrock::StopReason::MalformedModelOutput,
                StopReason::Other("malformed_model_output".into()),
            ),
            (
                bedrock::StopReason::MalformedToolUse,
                StopReason::Other("malformed_tool_use".into()),
            ),
            (
                bedrock::StopReason::from("future_stop_reason"),
                StopReason::Other("future_stop_reason".into()),
            ),
        ]
    }

    fn converse_output(stop_reason: bedrock::StopReason) -> ConverseOutput {
        let message = bedrock::Message::builder()
            .role(bedrock::ConversationRole::Assistant)
            .content(bedrock::ContentBlock::Text("done".into()))
            .build()
            .unwrap();
        let usage = bedrock::TokenUsage::builder()
            .input_tokens(1)
            .output_tokens(1)
            .total_tokens(2)
            .build()
            .unwrap();
        let metrics = bedrock::ConverseMetrics::builder()
            .latency_ms(1)
            .build()
            .unwrap();
        ConverseOutput::builder()
            .output(bedrock::ConverseOutput::Message(message))
            .stop_reason(stop_reason)
            .usage(usage)
            .metrics(metrics)
            .build()
            .unwrap()
    }

    #[test]
    fn non_stream_response_maps_every_bedrock_stop_reason() {
        for (bedrock_reason, expected) in stop_reason_cases() {
            let response = convert_response(converse_output(bedrock_reason.clone())).unwrap();
            assert_eq!(
                response.stop_reason,
                expected,
                "unexpected mapping for {}",
                bedrock_reason.as_str()
            );
        }
    }

    #[test]
    fn convert_usage_surfaces_cache_tokens() {
        // Bedrock reports cache read/write tokens upstream
        // (`cacheReadInputTokens` / `cacheWriteInputTokens`); the converter
        // surfaces them as `cache_read_tokens` / `cache_creation_tokens` so the
        // usage rollup can bill them at the cache tiers. Bedrock's `inputTokens`
        // already excludes cache, so it maps straight to the uncached count.
        let usage = bedrock::TokenUsage::builder()
            .input_tokens(10)
            .output_tokens(5)
            .total_tokens(15)
            .cache_read_input_tokens(7)
            .cache_write_input_tokens(3)
            .build()
            .expect("token usage with all required fields builds");

        let converted = convert_usage(Some(usage));

        assert_eq!(converted.input_tokens, Some(10));
        assert_eq!(converted.output_tokens, Some(5));
        assert_eq!(converted.total_tokens, Some(15));
        assert_eq!(converted.cache_read_tokens, Some(7));
        assert_eq!(converted.cache_creation_tokens, Some(3));
        assert_eq!(converted.reasoning_output_tokens, None);
    }

    #[test]
    fn convert_usage_defaults_when_absent() {
        // A response with no usage block yields a default (all-`None`) Usage.
        assert_eq!(convert_usage(None), Usage::default());
    }

    // ── StreamAccumulator: no receiver needed, drives `observe()` directly ──

    fn text_delta(index: i32, text: &str) -> bedrock::ConverseStreamOutput {
        bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(index)
                .delta(bedrock::ContentBlockDelta::Text(text.to_string()))
                .build()
                .unwrap(),
        )
    }

    fn block_stop(index: i32) -> bedrock::ConverseStreamOutput {
        bedrock::ConverseStreamOutput::ContentBlockStop(
            bedrock::ContentBlockStopEvent::builder()
                .content_block_index(index)
                .build()
                .unwrap(),
        )
    }

    fn message_stop(reason: bedrock::StopReason) -> bedrock::ConverseStreamOutput {
        bedrock::ConverseStreamOutput::MessageStop(
            bedrock::MessageStopEvent::builder()
                .stop_reason(reason)
                .build()
                .unwrap(),
        )
    }

    fn metadata(input: i32, output: i32) -> bedrock::ConverseStreamOutput {
        let usage = bedrock::TokenUsage::builder()
            .input_tokens(input)
            .output_tokens(output)
            .total_tokens(input + output)
            .build()
            .unwrap();
        bedrock::ConverseStreamOutput::Metadata(
            bedrock::ConverseStreamMetadataEvent::builder()
                .usage(usage)
                .build(),
        )
    }

    fn drain(acc: &mut StreamAccumulator) -> Vec<Result<StreamEvent, GenerationError>> {
        acc.pending.drain(..).collect()
    }

    #[test]
    fn stream_response_maps_every_bedrock_stop_reason() {
        for (bedrock_reason, expected) in stop_reason_cases() {
            let mut acc = StreamAccumulator::new();
            acc.observe(message_stop(bedrock_reason.clone()));
            acc.observe(metadata(1, 1));
            let events = drain(&mut acc);
            let actual = events
                .into_iter()
                .find_map(|event| match event.unwrap() {
                    StreamEvent::MessageStop { stop_reason, .. } => Some(stop_reason),
                    _ => None,
                })
                .expect("metadata should flush a MessageStop event");
            assert_eq!(
                actual,
                expected,
                "unexpected mapping for {}",
                bedrock_reason.as_str()
            );
        }
    }

    #[test]
    fn text_block_start_is_inferred_from_the_first_delta() {
        // Bedrock sends no ContentBlockStart for text blocks — the adapter
        // must synthesize one from the first delta.
        let mut acc = StreamAccumulator::new();
        acc.observe(text_delta(0, "Hello"));
        acc.observe(text_delta(0, ", world"));
        acc.observe(block_stop(0));

        let events = drain(&mut acc);
        assert_eq!(events.len(), 4, "start + 2 deltas + stop: {events:?}");
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text
            }
        ));
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == "Hello"
        ));
        assert!(matches!(
            events[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { index: 0, delta: ContentBlockDelta::Text(t) } if t == ", world"
        ));
        assert!(matches!(
            events[3].as_ref().unwrap(),
            StreamEvent::ContentBlockStop { index: 0 }
        ));
    }

    #[test]
    fn tool_use_block_is_classified_from_the_start_event() {
        let mut acc = StreamAccumulator::new();
        let start = bedrock::ContentBlockStartEvent::builder()
            .content_block_index(0)
            .start(bedrock::ContentBlockStart::ToolUse(
                bedrock::ToolUseBlockStart::builder()
                    .tool_use_id("call_1")
                    .name("get_weather")
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap();
        acc.observe(bedrock::ConverseStreamOutput::ContentBlockStart(start));
        acc.observe(bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(bedrock::ContentBlockDelta::ToolUse(
                    bedrock::ToolUseBlockDelta::builder()
                        .input(r#"{"city":"London"}"#)
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        ));

        let events = drain(&mut acc);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall { id, name, .. }
            } if id == "call_1" && name == "get_weather"
        ));
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall { arguments }
            } if arguments == r#"{"city":"London"}"#
        ));
    }

    #[test]
    fn reasoning_block_carries_its_signature() {
        let mut acc = StreamAccumulator::new();
        acc.observe(bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(bedrock::ContentBlockDelta::ReasoningContent(
                    bedrock::ReasoningContentBlockDelta::Text("Let me think...".to_string()),
                ))
                .build()
                .unwrap(),
        ));
        acc.observe(bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(bedrock::ContentBlockDelta::ReasoningContent(
                    bedrock::ReasoningContentBlockDelta::Signature("sig-123".to_string()),
                ))
                .build()
                .unwrap(),
        ));

        let events = drain(&mut acc);
        assert_eq!(
            events.len(),
            3,
            "start + text delta + signature delta: {events:?}"
        );
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart {
                block: ContentBlockStartData::Reasoning,
                ..
            }
        ));
        assert!(matches!(
            events[1].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Reasoning(t), .. } if t == "Let me think..."
        ));
        assert!(matches!(
            events[2].as_ref().unwrap(),
            StreamEvent::ContentBlockDelta { delta: ContentBlockDelta::Signature(s), .. } if s == "sig-123"
        ));
    }

    #[test]
    fn redacted_reasoning_filters_the_entire_block() {
        let mut acc = StreamAccumulator::new();
        acc.observe(bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(1)
                .delta(bedrock::ContentBlockDelta::ReasoningContent(
                    bedrock::ReasoningContentBlockDelta::RedactedContent(
                        aws_smithy_types::Blob::new(vec![1, 2, 3]),
                    ),
                ))
                .build()
                .unwrap(),
        ));
        acc.observe(block_stop(1));
        // A different, non-filtered index should still pass through.
        acc.observe(text_delta(0, "hi"));

        let events = drain(&mut acc);
        assert_eq!(
            events.len(),
            2,
            "redacted block's start/delta/stop are all suppressed, only index 0's start+delta remain: {events:?}"
        );
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::ContentBlockStart { index: 0, .. }
        ));
    }

    #[test]
    fn metadata_flushes_message_stop_with_accumulated_usage() {
        let mut acc = StreamAccumulator::new();
        acc.observe(text_delta(0, "hi"));
        acc.observe(block_stop(0));
        acc.observe(message_stop(bedrock::StopReason::EndTurn));
        // MessageStop alone must not flush anything yet — usage arrives via Metadata.
        assert!(
            acc.pending
                .iter()
                .all(|e| !matches!(e, Ok(StreamEvent::MessageStop { .. }))),
            "MessageStop should be held until Metadata arrives"
        );

        acc.observe(metadata(10, 5));

        let events = drain(&mut acc);
        let stop = events
            .last()
            .expect("at least the MessageStop event")
            .as_ref()
            .unwrap();
        assert!(matches!(
            stop,
            StreamEvent::MessageStop { stop_reason: StopReason::EndTurn, usage, .. }
                if usage.input_tokens == Some(10) && usage.output_tokens == Some(5)
        ));
    }

    #[test]
    fn flush_message_stop_is_idempotent() {
        let mut acc = StreamAccumulator::new();
        acc.observe(message_stop(bedrock::StopReason::EndTurn));
        acc.observe(metadata(1, 1));
        acc.flush_message_stop(); // simulates a stray Ok(None) after Metadata

        let stops = acc
            .pending
            .iter()
            .filter(|e| matches!(e, Ok(StreamEvent::MessageStop { .. })))
            .count();
        assert_eq!(
            stops, 1,
            "a second flush must not emit a second MessageStop"
        );
    }

    #[test]
    fn stream_ending_without_metadata_still_flushes_a_message_stop() {
        // Defensive path: if Metadata never arrives, `Ok(None)` (end of
        // stream) still flushes whatever usage/stop_reason was accumulated,
        // rather than silently dropping the terminal event.
        let mut acc = StreamAccumulator::new();
        acc.observe(message_stop(bedrock::StopReason::ToolUse));
        acc.flush_message_stop();

        let events = drain(&mut acc);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                ..
            }
        ));
    }
}
