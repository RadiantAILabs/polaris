//! Tracing decorator for [`DynLlmProvider`].
//!
//! [`TracingLlmProvider`] decorates any provider with OpenTelemetry-compatible
//! `chat` spans following the `GenAI` semantic conventions.

use super::genai_content;
use polaris_models::llm::{
    DynLlmProvider, GenerationError, LlmRequest, LlmResponse, LlmStream, ModelPricing, StreamEvent,
    Usage,
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
            gen_ai.usage.cost_usd = tracing::field::Empty,
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

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.inner.pricing(model)
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
                        record_cost(&current, inner.pricing(model), &response.usage);
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

                let pricing = inner.pricing(model);
                match inner.stream(model, request).await {
                    Ok(stream) => {
                        let wrapped: LlmStream = Box::pin(TracingStream {
                            inner: stream,
                            span: tracing::Span::current(),
                            pricing,
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

/// Records the estimated USD cost as `gen_ai.usage.cost_usd` on `span`.
///
/// No-ops when the provider has no pricing for the model or both token
/// counts are missing — partial counts (only input *or* only output known)
/// still produce a number using zero for the missing side, matching how
/// the aggregator at read-time treats `None`.
fn record_cost(span: &tracing::Span, pricing: Option<ModelPricing>, usage: &Usage) {
    let Some(rate) = pricing else { return };
    if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
        return;
    }
    let cost = rate.cost_with_cache(
        usage.input_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
        usage.cache_read_tokens.unwrap_or(0),
        usage.cache_creation_tokens.unwrap_or(0),
    );
    span.record("gen_ai.usage.cost_usd", cost);
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
    pricing: Option<ModelPricing>,
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
                    record_cost(&span, self.pricing, usage);
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

        fn pricing(&self, _model: &str) -> Option<ModelPricing> {
            None
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
        while let Poll::Ready(Some(item)) = stream.as_mut().poll_next(&mut cx) {
            items.push(item);
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
                        ..Default::default()
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

    // ──────────────────────────────────────────────────────────────────
    // record_cost end-to-end
    // ──────────────────────────────────────────────────────────────────

    use crate::tracing_plugin::{RecordingLayer, SpanRecord, SpanRecordSink};
    use polaris_models::llm::AssistantBlock;
    use tracing_subscriber::layer::SubscriberExt;

    /// Capturing sink that records every [`SpanRecord`] it sees.
    struct CapturingSink {
        records: Arc<Mutex<Vec<SpanRecord>>>,
    }

    impl SpanRecordSink for CapturingSink {
        fn push(&self, record: SpanRecord) {
            self.records.lock().push(record);
        }
    }

    /// Stub provider whose `generate()` returns a known [`LlmResponse`]
    /// and whose `pricing()` returns a fixed rate.
    struct PricingProvider {
        rate: ModelPricing,
        input_tokens: u64,
        output_tokens: u64,
    }

    impl DynLlmProvider for PricingProvider {
        fn name(&self) -> &'static str {
            "pricing-stub"
        }

        fn generate<'a>(
            &'a self,
            _model: &'a str,
            _request: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>>
        {
            let response = LlmResponse {
                content: Vec::<AssistantBlock>::new(),
                usage: Usage {
                    input_tokens: Some(self.input_tokens),
                    output_tokens: Some(self.output_tokens),
                    total_tokens: Some(self.input_tokens + self.output_tokens),
                    ..Default::default()
                },
                stop_reason: StopReason::EndTurn,
            };
            Box::pin(async move { Ok(response) })
        }

        fn stream<'a>(
            &'a self,
            _model: &'a str,
            _request: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
            Box::pin(async { unreachable!("not used in generate tests") })
        }

        fn pricing(&self, _model: &str) -> Option<ModelPricing> {
            Some(self.rate)
        }
    }

    /// End-to-end regression for [`record_cost`]: `generate()` on a
    /// provider that advertises pricing must emit `gen_ai.usage.cost_usd`
    /// on its chat span. Without this, a future change that drops
    /// `record_cost` from `TracingLlmProvider::generate` would silently
    /// stop populating per-call costs.
    #[tokio::test(flavor = "current_thread")]
    async fn generate_records_cost_usd_when_pricing_known() {
        let records = Arc::new(Mutex::new(Vec::<SpanRecord>::new()));
        let sink = Arc::new(CapturingSink {
            records: Arc::clone(&records),
        });
        let layer = RecordingLayer::with_sink(sink);
        let subscriber = tracing_subscriber::registry().with(layer);

        let provider = TracingLlmProvider::new(
            Arc::new(PricingProvider {
                rate: ModelPricing::new(3.0, 15.0),
                input_tokens: 1_000_000,
                output_tokens: 2_000_000,
            }),
            false,
        );

        let _guard = tracing::subscriber::set_default(subscriber);
        let response = provider
            .generate("test-model", empty_request())
            .await
            .expect("generate must succeed");
        assert_eq!(response.usage.input_tokens, Some(1_000_000));
        drop(_guard);

        let captured = records.lock();
        let chat = captured
            .iter()
            .find(|r| r.name == "chat")
            .expect("chat span must be recorded");

        let cost = chat
            .fields
            .get("gen_ai.usage.cost_usd")
            .expect("gen_ai.usage.cost_usd must be present when pricing is known")
            .as_f64()
            .expect("cost must serialize as a number");
        // 1_000_000 input @ $3/M + 2_000_000 output @ $15/M = $33.00
        assert!(
            (cost - 33.0).abs() < 1e-9,
            "expected cost ≈ 33.0 USD, got {cost}",
        );
    }
}
