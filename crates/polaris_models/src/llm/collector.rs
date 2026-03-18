//! Stream-to-response accumulation for [`StreamEvent`] streams.

use super::error::GenerationError;
use super::types::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, LlmResponse, ReasoningBlock,
    StreamEvent, TextBlock, ToolCall, ToolFunction, Usage,
};
use futures_core::Stream;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;

/// Extension methods for streams of [`StreamEvent`]s.
pub trait StreamEventExt: Stream<Item = Result<StreamEvent, GenerationError>> + Sized {
    /// Consume the stream and accumulate all events into an [`LlmResponse`].
    ///
    /// Only blocks that receive a [`StreamEvent::ContentBlockStop`] are included
    /// in the response. Blocks that are started but never stopped (e.g. due to a
    /// truncated stream) are silently discarded.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError`] if the stream yields an error
    /// event or terminates without a [`StreamEvent::MessageStop`].
    fn collect_response(self) -> impl Future<Output = Result<LlmResponse, GenerationError>>;
}

impl<S> StreamEventExt for S
where
    S: Stream<Item = Result<StreamEvent, GenerationError>> + Sized,
{
    async fn collect_response(self) -> Result<LlmResponse, GenerationError> {
        let mut stream = std::pin::pin!(self);
        let mut content: Vec<AssistantBlock> = Vec::new();
        let mut blocks: BTreeMap<u32, BlockAccumulator> = BTreeMap::new();
        let mut final_stop_reason = None;
        let mut final_usage = Usage::default();

        loop {
            let event = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
            match event {
                Some(Ok(event)) => match event {
                    StreamEvent::ContentBlockStart { index, block } => {
                        if blocks.contains_key(&index) {
                            return Err(GenerationError::InvalidResponse(format!(
                                "duplicate ContentBlockStart for index {index}"
                            )));
                        }
                        blocks.insert(index, BlockAccumulator::new(block));
                    }
                    StreamEvent::ContentBlockDelta { index, delta } => {
                        let acc = blocks.get_mut(&index).ok_or_else(|| {
                            GenerationError::InvalidResponse(format!(
                                "delta for unknown block index {index}"
                            ))
                        })?;
                        acc.apply_delta(delta)?;
                    }
                    StreamEvent::ContentBlockStop { index } => {
                        let acc = blocks.remove(&index).ok_or_else(|| {
                            GenerationError::InvalidResponse(format!(
                                "stop for unknown block index {index}"
                            ))
                        })?;
                        content.push(acc.into_block()?);
                    }
                    StreamEvent::MessageDelta { usage } => {
                        final_usage = usage;
                    }
                    StreamEvent::MessageStop { stop_reason, usage } => {
                        final_stop_reason = Some(stop_reason);
                        final_usage = usage;
                    }
                },
                Some(Err(err)) => return Err(err),
                None => break,
            }
        }

        let stop_reason = final_stop_reason.ok_or_else(|| {
            GenerationError::InvalidResponse("stream ended without MessageStop".to_owned())
        })?;

        Ok(LlmResponse {
            content,
            usage: final_usage,
            stop_reason,
        })
    }
}

/// Accumulates content block deltas into a complete [`AssistantBlock`].
enum BlockAccumulator {
    Text(String),
    ToolCall {
        id: String,
        call_id: Option<String>,
        name: String,
        arguments: String,
    },
    Reasoning(String),
}

impl BlockAccumulator {
    fn new(start: ContentBlockStartData) -> Self {
        match start {
            ContentBlockStartData::Text => Self::Text(String::new()),
            ContentBlockStartData::ToolCall { id, call_id, name } => Self::ToolCall {
                id,
                call_id,
                name,
                arguments: String::new(),
            },
            ContentBlockStartData::Reasoning => Self::Reasoning(String::new()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::ToolCall { .. } => "tool_call",
            Self::Reasoning(_) => "reasoning",
        }
    }

    fn apply_delta(&mut self, delta: ContentBlockDelta) -> Result<(), GenerationError> {
        #[expect(
            clippy::match_same_arms,
            reason = "arms enforce variant-to-variant pairing"
        )]
        match (self, &delta) {
            (Self::Text(buf), ContentBlockDelta::Text(s)) => buf.push_str(s),
            (Self::ToolCall { arguments, .. }, ContentBlockDelta::ToolCall { arguments: s }) => {
                arguments.push_str(s);
            }
            (Self::Reasoning(buf), ContentBlockDelta::Reasoning(s)) => buf.push_str(s),
            (acc, _) => {
                return Err(GenerationError::InvalidResponse(format!(
                    "delta type {delta_kind} does not match block type {block_kind}",
                    delta_kind = delta.kind(),
                    block_kind = acc.kind(),
                )));
            }
        }

        Ok(())
    }

    fn into_block(self) -> Result<AssistantBlock, GenerationError> {
        match self {
            Self::Text(text) => Ok(AssistantBlock::Text(TextBlock { text })),
            Self::ToolCall {
                id,
                call_id,
                name,
                arguments,
            } => {
                let arguments: Value = serde_json::from_str(&arguments).map_err(|err| {
                    GenerationError::InvalidResponse(format!(
                        "failed to parse tool call arguments: {err}"
                    ))
                })?;
                Ok(AssistantBlock::ToolCall(ToolCall {
                    id,
                    call_id,
                    function: ToolFunction { name, arguments },
                    signature: None,
                    additional_params: None,
                }))
            }
            Self::Reasoning(reasoning) => Ok(AssistantBlock::Reasoning(ReasoningBlock {
                id: None,
                reasoning: vec![reasoning],
                signature: None,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::StopReason;
    use futures_core::Stream;
    use serde_json::json;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A simple stream built from a `Vec` of items.
    struct EventStream(Vec<Result<StreamEvent, GenerationError>>);

    impl Stream for EventStream {
        type Item = Result<StreamEvent, GenerationError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if self.0.is_empty() {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(self.0.remove(0)))
            }
        }
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            total_tokens: Some(input + output),
        }
    }

    fn message_stop(stop_reason: StopReason, usage: Usage) -> StreamEvent {
        StreamEvent::MessageStop { stop_reason, usage }
    }

    #[tokio::test]
    async fn single_text_block() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("hello ".into()),
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("world".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::EndTurn, usage(10, 5))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert_eq!(
            response.text(),
            "hello world",
            "text deltas should be concatenated"
        );
        assert_eq!(
            response.stop_reason,
            StopReason::EndTurn,
            "stop reason should propagate"
        );
        assert_eq!(response.usage, usage(10, 5), "usage should propagate");
    }

    #[tokio::test]
    async fn tool_call_block() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall {
                    id: "tool_1".into(),
                    call_id: None,
                    name: "get_weather".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: r#"{"city""#.into(),
                },
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: r#":"London"}"#.into(),
                },
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::ToolUse, usage(10, 20))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");
        let calls = response.tool_calls();

        assert_eq!(calls.len(), 1, "should have one tool call");
        assert_eq!(calls[0].id, "tool_1", "tool call id should match");
        assert_eq!(
            calls[0].function.name, "get_weather",
            "tool name should match"
        );
        assert_eq!(
            calls[0].function.arguments,
            json!({"city": "London"}),
            "argument deltas should be concatenated and parsed as JSON"
        );
    }

    #[tokio::test]
    async fn reasoning_block() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("let me ".into()),
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("think...".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::EndTurn, usage(5, 5))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert!(
            matches!(&response.content[0], AssistantBlock::Reasoning(r) if r.reasoning == vec!["let me think..."]),
            "reasoning deltas should be concatenated"
        );
    }

    #[tokio::test]
    async fn mixed_block_types() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("thinking".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::ContentBlockStart {
                index: 1,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 1,
                delta: ContentBlockDelta::Text("answer".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 1 }),
            Ok(StreamEvent::ContentBlockStart {
                index: 2,
                block: ContentBlockStartData::ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "search".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 2,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "{}".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockStop { index: 2 }),
            Ok(message_stop(StopReason::ToolUse, usage(10, 30))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert_eq!(response.content.len(), 3, "should have three blocks");
        assert!(
            matches!(&response.content[0], AssistantBlock::Reasoning(_)),
            "first block should be reasoning"
        );
        assert!(
            matches!(&response.content[1], AssistantBlock::Text(_)),
            "second block should be text"
        );
        assert!(
            matches!(&response.content[2], AssistantBlock::ToolCall(_)),
            "third block should be tool call"
        );
    }

    #[tokio::test]
    async fn usage_from_message_stop() {
        let stream = EventStream(vec![
            Ok(StreamEvent::MessageDelta { usage: usage(5, 2) }),
            Ok(message_stop(StopReason::EndTurn, usage(10, 8))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert_eq!(
            response.usage,
            usage(10, 8),
            "final usage should come from MessageStop, not MessageDelta"
        );
    }

    #[tokio::test]
    async fn mid_stream_error() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Err(GenerationError::Http("connection reset".into())),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should propagate mid-stream error");

        assert!(
            matches!(err, GenerationError::Http(msg) if msg == "connection reset"),
            "should propagate the original error"
        );
    }

    #[tokio::test]
    async fn missing_message_stop() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("partial".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error without MessageStop");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("MessageStop")),
            "should indicate missing MessageStop"
        );
    }

    #[tokio::test]
    async fn invalid_tool_call_json() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "broken".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::ToolCall {
                    arguments: "not valid json".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::ToolUse, usage(5, 5))),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on invalid JSON");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("tool call arguments")),
            "should indicate JSON parse failure"
        );
    }

    #[tokio::test]
    async fn delta_for_unknown_index() {
        let stream = EventStream(vec![Ok(StreamEvent::ContentBlockDelta {
            index: 99,
            delta: ContentBlockDelta::Text("orphan".into()),
        })]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on unknown block index");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("unknown block index 99")),
            "should report the unknown index"
        );
    }

    #[tokio::test]
    async fn stop_for_unknown_index() {
        let stream = EventStream(vec![Ok(StreamEvent::ContentBlockStop { index: 42 })]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on unknown block index");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("unknown block index 42")),
            "should report the unknown index"
        );
    }

    #[tokio::test]
    async fn unfinished_block_discarded() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("partial".into()),
            }),
            // no ContentBlockStop for index 0
            Ok(message_stop(StopReason::EndTurn, usage(5, 5))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert!(
            response.content.is_empty(),
            "block without ContentBlockStop should be silently discarded"
        );
    }

    #[tokio::test]
    async fn text_block_no_deltas() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::EndTurn, usage(5, 0))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert_eq!(
            response.text(),
            "",
            "text block with no deltas should be empty"
        );
    }

    #[tokio::test]
    async fn tool_call_block_no_deltas() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::ToolCall {
                    id: "t1".into(),
                    call_id: None,
                    name: "noop".into(),
                },
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::ToolUse, usage(5, 0))),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on empty arguments");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("tool call arguments")),
            "empty arguments should fail JSON parsing"
        );
    }

    #[tokio::test]
    async fn reasoning_block_no_deltas() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(message_stop(StopReason::EndTurn, usage(5, 0))),
        ]);

        let response = stream.collect_response().await.expect("should succeed");

        assert!(
            matches!(&response.content[0], AssistantBlock::Reasoning(r) if r.reasoning == vec![""]),
            "reasoning block with no deltas should have empty content"
        );
    }

    #[tokio::test]
    async fn mismatched_delta_type() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Reasoning("wrong".into()),
            }),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on mismatched delta type");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("does not match")),
            "should report the type mismatch"
        );
    }

    #[tokio::test]
    async fn duplicate_content_block_start() {
        let stream = EventStream(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Reasoning,
            }),
        ]);

        let err = stream
            .collect_response()
            .await
            .expect_err("should error on duplicate start");

        assert!(
            matches!(&err, GenerationError::InvalidResponse(msg) if msg.contains("duplicate")),
            "should report the duplicate ContentBlockStart"
        );
    }
}
