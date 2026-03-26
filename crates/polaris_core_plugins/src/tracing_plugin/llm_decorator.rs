//! Tracing decorator for [`DynLlmProvider`].
//!
//! [`TracingLlmProvider`] decorates any provider with OpenTelemetry-compatible
//! `chat` spans following the `GenAI` semantic conventions.

use super::genai_content;
use polaris_models::llm::{
    DynLlmProvider, GenerationError, LlmRequest, LlmResponse, LlmStream, StreamEvent,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::Instrument;

/// Decorates a [`DynLlmProvider`] with tracing instrumentation.
///
/// Emits a `chat` span around every `generate()` and `stream()` call, recording
/// token usage, error status, and optionally message content.
///
/// For streaming calls, the span remains open for the lifetime of the returned
/// [`LlmStream`]. Token usage and stop reason are recorded when the
/// [`StreamEvent::MessageStop`] event is observed.
pub(crate) struct TracingLlmProvider {
    inner: Arc<dyn DynLlmProvider>,
    capture_genai_content: bool,
}

impl TracingLlmProvider {
    /// Creates a new tracing decorator.
    pub(crate) fn new(inner: Arc<dyn DynLlmProvider>, capture_genai_content: bool) -> Self {
        Self {
            inner,
            capture_genai_content,
        }
    }

    /// Builds the common `chat` span used by both `generate()` and `stream()`.
    fn chat_span(&self, model: &str, request: &LlmRequest) -> tracing::Span {
        let span_name = format!("chat {model}");
        let output_type = if request.output_schema.is_some() {
            "json"
        } else {
            "text"
        };
        tracing::info_span!(
            "chat",
            otel.name = %span_name,
            otel.kind = "Client",
            gen_ai.operation.name = "chat",
            gen_ai.output.type = output_type,
            gen_ai.provider.name = %self.inner.name(),
            gen_ai.request.model = %model,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
            gen_ai.system_instructions = tracing::field::Empty,
            gen_ai.tool.definitions = tracing::field::Empty,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        )
    }

    /// Records input content attributes on the current span.
    fn record_input_content(request: &LlmRequest) {
        let current = tracing::Span::current();
        current.record(
            "gen_ai.input.messages",
            genai_content::serialize_input_messages(&request.messages).as_str(),
        );
        if let Some(system) = &request.system {
            current.record(
                "gen_ai.system_instructions",
                genai_content::serialize_system_instructions(system).as_str(),
            );
        }
        if let Some(tools) = &request.tools {
            current.record(
                "gen_ai.tool.definitions",
                genai_content::serialize_tool_definitions(tools).as_str(),
            );
        }
    }
}

impl DynLlmProvider for TracingLlmProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn generate<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>> {
        let span = self.chat_span(model, &request);
        let capture_genai_content = self.capture_genai_content;
        let inner = Arc::clone(&self.inner);

        Box::pin(
            async move {
                if capture_genai_content {
                    Self::record_input_content(&request);
                }

                let result: Result<LlmResponse, GenerationError> =
                    inner.generate(model, request).await;

                match &result {
                    Ok(response) => {
                        let current = tracing::Span::current();
                        if let Some(input) = response.usage.input_tokens {
                            current.record("gen_ai.usage.input_tokens", input);
                        }
                        if let Some(output) = response.usage.output_tokens {
                            current.record("gen_ai.usage.output_tokens", output);
                        }
                        if capture_genai_content {
                            current.record(
                                "gen_ai.output.messages",
                                genai_content::serialize_output_messages(
                                    &response.content,
                                    &response.stop_reason,
                                )
                                .as_str(),
                            );
                        }
                    }
                    Err(gen_err) => {
                        let current = tracing::Span::current();
                        current.record("error.type", gen_err.error_type());
                        current.record("otel.status_code", "ERROR");
                        current.record("otel.status_description", gen_err.to_string().as_str());
                    }
                }

                result
            }
            .instrument(span),
        )
    }

    fn stream<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
        let span = self.chat_span(model, &request);
        let capture_genai_content = self.capture_genai_content;
        let inner = Arc::clone(&self.inner);

        Box::pin(
            async move {
                if capture_genai_content {
                    Self::record_input_content(&request);
                }

                match inner.stream(model, request).await {
                    Ok(stream) => {
                        let wrapped: LlmStream = Box::pin(TracingStream {
                            inner: stream,
                            span: tracing::Span::current(),
                        });
                        Ok(wrapped)
                    }
                    Err(gen_err) => {
                        let current = tracing::Span::current();
                        current.record("error.type", gen_err.error_type());
                        current.record("otel.status_code", "ERROR");
                        current.record("otel.status_description", gen_err.to_string().as_str());
                        Err(gen_err)
                    }
                }
            }
            .instrument(span),
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TracingStream
// ─────────────────────────────────────────────────────────────────────────────

/// Wrapper stream that keeps a `chat` span alive and records final metrics.
///
/// Each `poll_next` is executed within the span so that any tracing events
/// emitted by the inner stream are correctly parented. When
/// [`StreamEvent::MessageStop`] arrives, token usage is recorded on the span.
struct TracingStream {
    inner: LlmStream,
    span: tracing::Span,
}

impl futures_core::Stream for TracingStream {
    type Item = Result<StreamEvent, GenerationError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let span = self.span.clone();
        let _enter = span.enter();

        let poll = self.inner.as_mut().poll_next(cx);

        match &poll {
            Poll::Ready(Some(Ok(event))) => {
                if let StreamEvent::MessageStop {
                    stop_reason: _,
                    usage,
                } = event
                {
                    if let Some(input) = usage.input_tokens {
                        span.record("gen_ai.usage.input_tokens", input);
                    }
                    if let Some(output) = usage.output_tokens {
                        span.record("gen_ai.usage.output_tokens", output);
                    }
                }
            }
            Poll::Ready(Some(Err(gen_err))) => {
                span.record("error.type", gen_err.error_type());
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_description", gen_err.to_string().as_str());
            }
            Poll::Ready(None) | Poll::Pending => {}
        }

        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use polaris_models::llm::{StopReason, Usage};
    use std::collections::VecDeque;

    /// A simple stream that yields items from a `VecDeque`.
    struct VecStream(VecDeque<Result<StreamEvent, GenerationError>>);

    impl futures_core::Stream for VecStream {
        type Item = Result<StreamEvent, GenerationError>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    /// Stub provider whose stream contents are taken from a `Mutex`
    /// (avoids requiring `Clone` on `GenerationError`).
    struct StubStreamProvider {
        events: Mutex<Option<VecDeque<Result<StreamEvent, GenerationError>>>>,
    }

    impl StubStreamProvider {
        fn new(events: Vec<Result<StreamEvent, GenerationError>>) -> Self {
            Self {
                events: Mutex::new(Some(events.into())),
            }
        }
    }

    impl DynLlmProvider for StubStreamProvider {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn generate<'a>(
            &'a self,
            _model: &'a str,
            _request: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>>
        {
            Box::pin(async { unreachable!("not used in stream tests") })
        }

        fn stream<'a>(
            &'a self,
            _model: &'a str,
            _request: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
            let events = self
                .events
                .lock()
                .take()
                .expect("stream() called more than once");
            Box::pin(async move { Ok(Box::pin(VecStream(events)) as LlmStream) })
        }
    }

    fn empty_request() -> LlmRequest {
        LlmRequest::default()
    }

    /// Drains a stream synchronously (all items are immediately ready).
    fn drain_stream(stream: &mut LlmStream) -> Vec<Result<StreamEvent, GenerationError>> {
        let waker = &std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut items = vec![];
        loop {
            match stream.as_mut().poll_next(&mut cx) {
                Poll::Ready(Some(item)) => items.push(item),
                Poll::Ready(None) => break,
                Poll::Pending => break,
            }
        }
        items
    }

    #[tokio::test]
    async fn stream_yields_all_events_through_tracing_wrapper() {
        let provider = TracingLlmProvider::new(
            Arc::new(StubStreamProvider::new(vec![Ok(
                StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: Some(10),
                        output_tokens: Some(20),
                        total_tokens: Some(30),
                    },
                },
            )])),
            false,
        );
        let mut stream = provider
            .stream("test-model", empty_request())
            .await
            .unwrap();

        let items = drain_stream(&mut stream);
        assert_eq!(items.len(), 1);
        match &items[0] {
            Ok(StreamEvent::MessageStop { usage, .. }) => {
                assert_eq!(usage.input_tokens, Some(10));
                assert_eq!(usage.output_tokens, Some(20));
            }
            other => panic!("expected MessageStop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stream_propagates_errors() {
        let provider = TracingLlmProvider::new(
            Arc::new(StubStreamProvider::new(vec![Err(
                GenerationError::UnsupportedOperation("test error".to_string()),
            )])),
            false,
        );
        let mut stream = provider
            .stream("test-model", empty_request())
            .await
            .unwrap();

        let items = drain_stream(&mut stream);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }
}
