//! Tracing decorator for [`DynLlmProvider`].
//!
//! [`TracingLlmProvider`] decorates any provider with OpenTelemetry-compatible
//! `chat` spans following the `GenAI` semantic conventions.

use super::genai_content;
use polaris_models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, DynLlmProvider, GenerationError,
    LlmRequest, LlmResponse, LlmStream, ModelPricing, ReasoningBlock, StopReason, StreamEvent,
    TextBlock, ToolCall, ToolFunction, Usage,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
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
    fn chat_span(&self, model: &str, request: &LlmRequest, stream: bool) -> tracing::Span {
        let span_name = format!("chat {model}");
        let span = tracing::info_span!(
            "chat",
            otel.name = %span_name,
            otel.kind = "Client",
            gen_ai.operation.name = "chat",
            gen_ai.provider.name = %self.inner.name(),
            error.type = tracing::field::Empty,
            gen_ai.output.type = tracing::field::Empty,
            gen_ai.request.model = %model,
            gen_ai.request.stream = stream,
            gen_ai.response.finish_reasons = tracing::field::Empty,
            gen_ai.response.id = tracing::field::Empty,
            gen_ai.response.model = tracing::field::Empty,
            gen_ai.response.time_to_first_chunk = tracing::field::Empty,
            gen_ai.usage.cache_creation.input_tokens = tracing::field::Empty,
            gen_ai.usage.cache_read.input_tokens = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.reasoning.output_tokens = tracing::field::Empty,
            server.address = tracing::field::Empty,
            gen_ai.input.messages = tracing::field::Empty,
            gen_ai.output.messages = tracing::field::Empty,
            gen_ai.system_instructions = tracing::field::Empty,
            gen_ai.tool.definitions = tracing::field::Empty,
            polaris.gen_ai.cost_usd = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        );

        if request.output_schema.is_some() {
            span.record("gen_ai.output.type", "json");
        }
        if let Some(endpoint) = self.inner.endpoint() {
            span.record("server.address", endpoint.as_str());
        }

        span
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

    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }

    fn generate<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>> {
        let span = self.chat_span(model, &request, false);
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
                        record_usage(&current, &response.usage);
                        record_cost(&current, inner.pricing(model), &response.usage);
                        record_finish_reasons(&current, &response.stop_reason);
                        if let Some(id) = &response.id {
                            current.record("gen_ai.response.id", id.as_str());
                        }
                        if let Some(response_model) = &response.model {
                            current.record("gen_ai.response.model", response_model.as_str());
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
        let span = self.chat_span(model, &request, true);
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
                            capture_genai_content,
                            started: Instant::now(),
                            first_chunk_seen: false,
                            output: StreamOutputAccumulator::new(),
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

/// Records the estimated USD cost as `polaris.gen_ai.cost_usd` on `span`.
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
    span.record("polaris.gen_ai.cost_usd", cost);
}

/// Records token-usage attributes from `usage` on `span`.
fn record_usage(span: &tracing::Span, usage: &Usage) {
    if let Some(input) = usage.input_tokens {
        span.record("gen_ai.usage.input_tokens", input);
    }
    if let Some(output) = usage.output_tokens {
        span.record("gen_ai.usage.output_tokens", output);
    }
    if let Some(cache_creation) = usage.cache_creation_tokens {
        span.record("gen_ai.usage.cache_creation.input_tokens", cache_creation);
    }
    if let Some(cache_read) = usage.cache_read_tokens {
        span.record("gen_ai.usage.cache_read.input_tokens", cache_read);
    }
    if let Some(reasoning) = usage.reasoning_output_tokens {
        span.record("gen_ai.usage.reasoning.output_tokens", reasoning);
    }
}

/// Records `gen_ai.response.finish_reasons` from `stop_reason` on `span`.
fn record_finish_reasons(span: &tracing::Span, stop_reason: &StopReason) {
    span.record(
        "gen_ai.response.finish_reasons",
        genai_content::finish_reason(stop_reason),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// TracingStream
// ─────────────────────────────────────────────────────────────────────────────

/// Wrapper stream that keeps a `chat` span alive and records final metrics.
///
/// Each `poll_next` is executed within the span so that any tracing events
/// emitted by the inner stream are correctly parented. The first content
/// chunk records `gen_ai.response.time_to_first_chunk`; when
/// [`StreamEvent::MessageStop`] arrives, token usage, cost, finish reasons
/// and (when content capture is on) `gen_ai.output.messages` are recorded.
struct TracingStream {
    inner: LlmStream,
    span: tracing::Span,
    pricing: Option<ModelPricing>,
    capture_genai_content: bool,
    started: Instant,
    first_chunk_seen: bool,
    output: StreamOutputAccumulator,
}

impl futures_core::Stream for TracingStream {
    type Item = Result<StreamEvent, GenerationError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let span = self.span.clone();
        let _enter = span.enter();

        let poll = self.inner.as_mut().poll_next(cx);

        match &poll {
            Poll::Ready(Some(Ok(event))) => {
                if !self.first_chunk_seen && matches!(event, StreamEvent::ContentBlockDelta { .. })
                {
                    span.record(
                        "gen_ai.response.time_to_first_chunk",
                        self.started.elapsed().as_secs_f64(),
                    );
                    self.first_chunk_seen = true;
                }

                if self.capture_genai_content {
                    self.output.observe(event);
                }

                if let StreamEvent::MessageStop {
                    stop_reason,
                    usage,
                    id,
                    model,
                } = event
                {
                    record_usage(&span, usage);
                    record_cost(&span, self.pricing, usage);
                    record_finish_reasons(&span, stop_reason);
                    if let Some(id) = id {
                        span.record("gen_ai.response.id", id.as_str());
                    }
                    if let Some(model) = model {
                        span.record("gen_ai.response.model", model.as_str());
                    }
                    if self.capture_genai_content {
                        span.record(
                            "gen_ai.output.messages",
                            genai_content::serialize_output_messages(
                                &self.output.blocks(),
                                stop_reason,
                            )
                            .as_str(),
                        );
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

/// Reconstructs assistant content blocks from streaming events.
///
/// Only used when content capture is enabled, so the accumulated text/tool/
/// reasoning blocks can be serialized into `gen_ai.output.messages` once the
/// stream completes.
struct StreamOutputAccumulator {
    blocks: Vec<PartialBlock>,
}

/// A content block being reconstructed from streaming deltas.
enum PartialBlock {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Reasoning(String),
}

impl StreamOutputAccumulator {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Folds a single stream event into the accumulated blocks.
    fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ContentBlockStart { block, .. } => match block {
                ContentBlockStartData::Text => self.blocks.push(PartialBlock::Text(String::new())),
                ContentBlockStartData::ToolCall { id, name, .. } => {
                    self.blocks.push(PartialBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    });
                }
                ContentBlockStartData::Reasoning => {
                    self.blocks.push(PartialBlock::Reasoning(String::new()));
                }
            },
            StreamEvent::ContentBlockDelta { delta, .. } => {
                let Some(current) = self.blocks.last_mut() else {
                    return;
                };
                match (current, delta) {
                    (PartialBlock::Text(text), ContentBlockDelta::Text(chunk))
                    | (PartialBlock::Reasoning(text), ContentBlockDelta::Reasoning(chunk)) => {
                        text.push_str(chunk);
                    }
                    (
                        PartialBlock::ToolCall { arguments, .. },
                        ContentBlockDelta::ToolCall { arguments: chunk },
                    ) => arguments.push_str(chunk),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Materializes the accumulated partial blocks into [`AssistantBlock`]s.
    fn blocks(&self) -> Vec<AssistantBlock> {
        self.blocks
            .iter()
            .map(|block| match block {
                PartialBlock::Text(text) => AssistantBlock::Text(TextBlock { text: text.clone() }),
                PartialBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => AssistantBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    call_id: None,
                    function: ToolFunction {
                        name: name.clone(),
                        arguments: serde_json::from_str(arguments)
                            .unwrap_or_else(|_| serde_json::Value::String(arguments.clone())),
                    },
                    signature: None,
                    additional_params: None,
                }),
                PartialBlock::Reasoning(text) => AssistantBlock::Reasoning(ReasoningBlock {
                    id: None,
                    reasoning: vec![text.clone()],
                    signature: None,
                }),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use polaris_models::llm::{StopReason, Usage};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc as StdArc;
    use tracing_subscriber::layer::SubscriberExt as _;

    // ─────────────────────────────────────────────────────────────────────
    // Span-field capture
    //
    // The `chat` span declares every attribute as `Empty` at creation and
    // fills them in later via `span.record(...)`. To assert on those values
    // we install a minimal `tracing_subscriber::Layer` whose `on_record`
    // callback merges field values into a shared map. Tests run a single
    // `chat` span at a time, so one flat map (no per-span keying) suffices.
    // ─────────────────────────────────────────────────────────────────────

    /// Collected span attributes, keyed by field name.
    type Fields = StdArc<Mutex<HashMap<String, String>>>;

    /// Visitor that stringifies every recorded field into `fields`.
    struct FieldCollector<'a> {
        fields: &'a mut HashMap<String, String>,
    }

    impl tracing::field::Visit for FieldCollector<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    /// A `Layer` that records span field values (at creation and on `record`)
    /// into a shared map for later assertion.
    struct CaptureLayer {
        fields: Fields,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = self.fields.lock();
            attrs.record(&mut FieldCollector {
                fields: &mut fields,
            });
        }

        fn on_record(
            &self,
            _id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = self.fields.lock();
            values.record(&mut FieldCollector {
                fields: &mut fields,
            });
        }
    }

    /// Runs `f` with a `CaptureLayer` installed as the default subscriber and
    /// returns the captured span fields.
    fn capture_span_fields<F>(f: F) -> HashMap<String, String>
    where
        F: FnOnce(),
    {
        let fields: Fields = StdArc::new(Mutex::new(HashMap::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer {
            fields: StdArc::clone(&fields),
        });
        tracing::subscriber::with_default(subscriber, f);
        let guard = fields.lock();
        guard.clone()
    }

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
        pricing: Option<ModelPricing>,
        endpoint: Option<String>,
    }

    impl StubStreamProvider {
        fn new(events: Vec<Result<StreamEvent, GenerationError>>) -> Self {
            Self {
                events: Mutex::new(Some(events.into())),
                pricing: None,
                endpoint: None,
            }
        }

        /// Same as [`Self::new`] but with a provider endpoint, so the wrapper
        /// records it as `server.address`.
        fn with_endpoint(
            events: Vec<Result<StreamEvent, GenerationError>>,
            endpoint: &str,
        ) -> Self {
            Self {
                events: Mutex::new(Some(events.into())),
                pricing: None,
                endpoint: Some(endpoint.to_string()),
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
            self.pricing
        }

        fn endpoint(&self) -> Option<String> {
            self.endpoint.clone()
        }
    }

    fn empty_request() -> LlmRequest {
        LlmRequest::default()
    }

    /// Resolves an immediately-ready future synchronously.
    ///
    /// The stub provider's `stream()` future never yields `Pending`, so a
    /// single poll on a no-op waker is enough; this lets finalization tests run
    /// under a `with_default` subscriber without an async runtime.
    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("future unexpectedly pending"),
        }
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
                    id: None,
                    model: None,
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
        match &items[0] {
            Err(GenerationError::UnsupportedOperation(msg)) => {
                assert_eq!(
                    msg, "test error",
                    "the underlying provider's error must propagate unchanged"
                );
            }
            other => panic!("expected the injected UnsupportedOperation error, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // StreamOutputAccumulator helpers + builders
    // ─────────────────────────────────────────────────────────────────────

    fn block_start(index: u32, block: ContentBlockStartData) -> StreamEvent {
        StreamEvent::ContentBlockStart { index, block }
    }

    fn block_delta(index: u32, delta: ContentBlockDelta) -> StreamEvent {
        StreamEvent::ContentBlockDelta { index, delta }
    }

    /// Feeds every event into a fresh accumulator and returns its blocks.
    fn accumulate(events: &[StreamEvent]) -> Vec<AssistantBlock> {
        let mut acc = StreamOutputAccumulator::new();
        for event in events {
            acc.observe(event);
        }
        acc.blocks()
    }

    #[test]
    fn accumulator_reconstructs_text_across_deltas() {
        let blocks = accumulate(&[
            block_start(0, ContentBlockStartData::Text),
            block_delta(0, ContentBlockDelta::Text("Hello, ".to_string())),
            block_delta(0, ContentBlockDelta::Text("world".to_string())),
        ]);

        assert_eq!(blocks.len(), 1, "one text block should be reconstructed");
        match &blocks[0] {
            AssistantBlock::Text(text) => assert_eq!(
                text.text, "Hello, world",
                "text deltas should concatenate in order"
            ),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_reconstructs_tool_call_name_and_arguments() {
        let blocks = accumulate(&[
            block_start(
                0,
                ContentBlockStartData::ToolCall {
                    id: "call_1".to_string(),
                    call_id: None,
                    name: "get_weather".to_string(),
                },
            ),
            block_delta(
                0,
                ContentBlockDelta::ToolCall {
                    arguments: "{\"city\":".to_string(),
                },
            ),
            block_delta(
                0,
                ContentBlockDelta::ToolCall {
                    arguments: "\"London\"}".to_string(),
                },
            ),
        ]);

        assert_eq!(
            blocks.len(),
            1,
            "one tool-call block should be reconstructed"
        );
        match &blocks[0] {
            AssistantBlock::ToolCall(call) => {
                assert_eq!(call.id, "call_1", "tool-call id should be preserved");
                assert_eq!(
                    call.function.name, "get_weather",
                    "tool-call name should be preserved"
                );
                assert_eq!(
                    call.function.arguments,
                    serde_json::json!({"city": "London"}),
                    "streamed argument fragments should parse into JSON once joined"
                );
            }
            other => panic!("expected a tool-call block, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_reconstructs_reasoning_across_deltas() {
        let blocks = accumulate(&[
            block_start(0, ContentBlockStartData::Reasoning),
            block_delta(0, ContentBlockDelta::Reasoning("Let me ".to_string())),
            block_delta(0, ContentBlockDelta::Reasoning("think.".to_string())),
        ]);

        assert_eq!(
            blocks.len(),
            1,
            "one reasoning block should be reconstructed"
        );
        match &blocks[0] {
            AssistantBlock::Reasoning(reasoning) => assert_eq!(
                reasoning.reasoning,
                vec!["Let me think.".to_string()],
                "reasoning deltas should concatenate into a single fragment"
            ),
            other => panic!("expected a reasoning block, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_ignores_delta_without_matching_start() {
        // A delta whose payload kind does not match the current open block
        // falls through `_ => {}` and is dropped; the started text block stays
        // empty rather than absorbing a mismatched tool-call fragment.
        let blocks = accumulate(&[
            block_start(0, ContentBlockStartData::Text),
            block_delta(
                0,
                ContentBlockDelta::ToolCall {
                    arguments: "ignored".to_string(),
                },
            ),
            // A signature delta has no matching arm at all (also `_ => {}`).
            block_delta(0, ContentBlockDelta::Signature("sig".to_string())),
        ]);

        assert_eq!(blocks.len(), 1, "the text block should still be present");
        match &blocks[0] {
            AssistantBlock::Text(text) => assert_eq!(
                text.text, "",
                "mismatched deltas should not be appended to the text block"
            ),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_drops_delta_with_no_open_block() {
        // A delta arriving before any `ContentBlockStart` hits the
        // `blocks.last_mut()` early return; nothing is accumulated.
        let blocks = accumulate(&[block_delta(
            0,
            ContentBlockDelta::Text("orphan".to_string()),
        )]);

        assert!(
            blocks.is_empty(),
            "a delta with no preceding block-start should produce no blocks"
        );
    }

    #[test]
    fn accumulator_empty_returns_no_blocks() {
        // An accumulator that never observed anything yields nothing, and a
        // started-but-empty block materializes as an empty text block.
        assert!(
            accumulate(&[]).is_empty(),
            "an untouched accumulator should have no blocks"
        );

        let started_only = accumulate(&[block_start(0, ContentBlockStartData::Text)]);
        assert_eq!(
            started_only.len(),
            1,
            "a block-start with no deltas still materializes one block"
        );
        match &started_only[0] {
            AssistantBlock::Text(text) => assert_eq!(
                text.text, "",
                "a started-but-empty text block has empty text"
            ),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn accumulator_malformed_tool_arguments_fall_back_to_raw_string() {
        // When the streamed argument fragments do not form valid JSON, the
        // `serde_json::from_str(...).unwrap_or_else(...)` fallback in `blocks()`
        // must yield the raw text as a JSON string rather than panicking.
        let blocks = accumulate(&[
            block_start(
                0,
                ContentBlockStartData::ToolCall {
                    id: "call_bad".to_string(),
                    call_id: None,
                    name: "broken".to_string(),
                },
            ),
            block_delta(
                0,
                ContentBlockDelta::ToolCall {
                    arguments: "{not valid json".to_string(),
                },
            ),
        ]);

        assert_eq!(
            blocks.len(),
            1,
            "the tool-call block should still be present"
        );
        match &blocks[0] {
            AssistantBlock::ToolCall(call) => assert_eq!(
                call.function.arguments,
                serde_json::Value::String("{not valid json".to_string()),
                "malformed JSON arguments should fall back to the raw string, not panic"
            ),
            other => panic!("expected a tool-call block, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // TracingStream finalization (content capture on)
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn stream_records_finalization_fields_with_content_capture() {
        // A content stream: text deltas, then a tool call, terminated by a
        // populated MessageStop. With capture enabled, finalization should
        // record response id/model, finish reason, time-to-first-chunk, and
        // the serialized output messages, plus the provider endpoint as
        // server.address.
        let events = vec![
            Ok(block_start(0, ContentBlockStartData::Text)),
            Ok(block_delta(
                0,
                ContentBlockDelta::Text("Calling ".to_string()),
            )),
            Ok(block_delta(
                0,
                ContentBlockDelta::Text("a tool".to_string()),
            )),
            Ok(block_start(
                1,
                ContentBlockStartData::ToolCall {
                    id: "call_42".to_string(),
                    call_id: None,
                    name: "search".to_string(),
                },
            )),
            Ok(block_delta(
                1,
                ContentBlockDelta::ToolCall {
                    arguments: "{\"q\":\"rust\"}".to_string(),
                },
            )),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: Some(7),
                    output_tokens: Some(11),
                    ..Default::default()
                },
                id: Some("resp_abc".to_string()),
                model: Some("test-model-v2".to_string()),
            }),
        ];

        let fields = capture_span_fields(|| {
            let provider = TracingLlmProvider::new(
                Arc::new(StubStreamProvider::with_endpoint(
                    events,
                    "https://api.example.test/v1",
                )),
                true,
            );
            // The provider's stream future and the wrapper poll loop must run
            // within the captured subscriber; drive them synchronously on the
            // current thread (the stub future is immediately ready).
            let mut stream = block_on_ready(provider.stream("test-model", empty_request()))
                .expect("stream should be constructed");
            let items = drain_stream(&mut stream);
            assert_eq!(
                items.len(),
                6,
                "every event should pass through the tracing wrapper"
            );
        });

        assert_eq!(
            fields.get("gen_ai.response.id").map(String::as_str),
            Some("resp_abc"),
            "the response id from MessageStop should be recorded"
        );
        assert_eq!(
            fields.get("gen_ai.response.model").map(String::as_str),
            Some("test-model-v2"),
            "the response model from MessageStop should be recorded"
        );
        assert_eq!(
            fields
                .get("gen_ai.response.finish_reasons")
                .map(String::as_str),
            Some("tool_call"),
            "ToolUse stop reason should map to the tool_call finish reason"
        );
        assert!(
            fields.contains_key("gen_ai.response.time_to_first_chunk"),
            "the first content delta should record time_to_first_chunk \
             (value is wall-clock, so only presence is asserted)"
        );
        assert_eq!(
            fields.get("server.address").map(String::as_str),
            Some("https://api.example.test/v1"),
            "the provider endpoint should surface as server.address"
        );

        let output = fields
            .get("gen_ai.output.messages")
            .expect("gen_ai.output.messages should be recorded under content capture");
        let parsed: serde_json::Value =
            serde_json::from_str(output).expect("output.messages should be valid JSON");
        assert_eq!(
            parsed[0]["role"], "assistant",
            "the reconstructed output is an assistant message"
        );
        assert_eq!(
            parsed[0]["finish_reason"], "tool_call",
            "the serialized output carries the finish reason"
        );
        assert_eq!(
            parsed[0]["parts"][0]["type"], "text",
            "the first reconstructed part is the accumulated text"
        );
        assert_eq!(
            parsed[0]["parts"][0]["content"], "Calling a tool",
            "streamed text deltas should be reassembled in order"
        );
        assert_eq!(
            parsed[0]["parts"][1]["type"], "tool_call",
            "the second reconstructed part is the tool call"
        );
        assert_eq!(
            parsed[0]["parts"][1]["name"], "search",
            "the tool-call name should round-trip"
        );
        assert_eq!(
            parsed[0]["parts"][1]["arguments"]["q"], "rust",
            "the streamed tool-call arguments should round-trip as JSON"
        );
    }
}
