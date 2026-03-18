//! Single-shot request builder for [`Llm`].
//!
//! Provides an ergonomic builder for assembling and sending a single LLM call
//! with tool definitions, messages, and options.
//!
//! Uses typestate to enforce at compile time that at least one message is
//! present before a request can be sent.
//!
//! # Example
//!
//! ```no_run
//! use polaris_models::ModelRegistry;
//! use polaris_models::llm::Llm;
//!
//! # async fn example(llm: Llm) -> Result<(), Box<dyn std::error::Error>> {
//! let response = llm
//!     .builder()
//!     .system("You are helpful")
//!     .user("What's the weather?")
//!     .generate()
//!     .await?;
//! # Ok(())
//! # }
//! ```

use super::error::{ExtractionError, GenerationError};
use super::model::Llm;
use super::types::{LlmRequest, LlmResponse, LlmStream, Message, ToolChoice, ToolDefinition};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;

/// Typestate marker: the builder has no messages yet.
pub struct Empty {
    _private: (),
}

/// Typestate marker: the builder has at least one message.
pub struct Ready {
    _private: (),
}

/// A builder for single-shot LLM requests.
///
/// Created via [`Llm::builder()`]. Uses typestate to enforce that at least one
/// message is added before sending. Terminal methods ([`generate()`](Self::generate),
/// [`generate_structured()`](Self::generate_structured)) are only available in
/// the [`Ready`] state.
pub struct LlmRequestBuilder<'a, S = Empty> {
    llm: &'a Llm,
    tools: Vec<ToolDefinition>,
    system: Option<String>,
    messages: Vec<Message>,
    tool_choice: Option<ToolChoice>,
    _state: PhantomData<S>,
}

// ─────────────────────
// Methods available in any state
// ─────────────────────

impl<'a, S> LlmRequestBuilder<'a, S> {
    /// Returns the number of tool definitions currently accumulated.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Adds tool definitions directly.
    ///
    /// Can be called multiple times; definitions accumulate.
    #[must_use]
    pub fn with_definitions(mut self, definitions: Vec<ToolDefinition>) -> Self {
        self.tools.extend(definitions);
        self
    }

    /// Sets the system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Sets how the model should choose tools.
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Requires the model to call at least one tool.
    ///
    /// Shorthand for `.tool_choice(ToolChoice::Required)`.
    #[must_use]
    pub fn require_tool(mut self) -> Self {
        self.tool_choice = Some(ToolChoice::Required);
        self
    }

    /// Allows the model to decide whether to call tools.
    ///
    /// Shorthand for `.tool_choice(ToolChoice::Auto)`.
    #[must_use]
    pub fn auto_tool(mut self) -> Self {
        self.tool_choice = Some(ToolChoice::Auto);
        self
    }

    /// Disallows the model from calling any tools.
    ///
    /// Shorthand for `.tool_choice(ToolChoice::None)`.
    #[must_use]
    pub fn no_tool(mut self) -> Self {
        self.tool_choice = Some(ToolChoice::None);
        self
    }
}

// ─────────────────────
// Message methods (transition any state → Ready)
// ─────────────────────

impl<'a, S> LlmRequestBuilder<'a, S> {
    /// Converts internal state to a new typestate.
    fn transition<T>(self) -> LlmRequestBuilder<'a, T> {
        LlmRequestBuilder {
            llm: self.llm,
            tools: self.tools,
            system: self.system,
            messages: self.messages,
            tool_choice: self.tool_choice,
            _state: PhantomData,
        }
    }

    /// Sets the conversation messages, replacing any existing messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<Message>) -> LlmRequestBuilder<'a, Ready> {
        self.messages = messages;
        self.transition()
    }

    /// Appends a single message to the conversation.
    #[must_use]
    pub fn message(mut self, message: Message) -> LlmRequestBuilder<'a, Ready> {
        self.messages.push(message);
        self.transition()
    }

    /// Appends a user message with text content.
    #[must_use]
    pub fn user(self, text: impl Into<String>) -> LlmRequestBuilder<'a, Ready> {
        self.message(Message::user(text))
    }

    /// Appends an assistant message with text content.
    #[must_use]
    pub fn assistant(self, text: impl Into<String>) -> LlmRequestBuilder<'a, Ready> {
        self.message(Message::assistant(text))
    }
}

// ─────────────────────
// Terminal methods (only in Ready state)
// ─────────────────────

impl<'a> LlmRequestBuilder<'a, Ready> {
    /// Builds the [`LlmRequest`] from the accumulated state.
    fn build(self) -> (&'a Llm, LlmRequest) {
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(self.tools)
        };

        let request = LlmRequest {
            system: self.system,
            messages: self.messages,
            tools,
            tool_choice: self.tool_choice,
            output_schema: None,
        };

        (self.llm, request)
    }

    /// Sends the generation request and returns the raw response.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError`] if the underlying LLM call fails.
    pub async fn generate(self) -> Result<LlmResponse, GenerationError> {
        let (llm, request) = self.build();
        llm.generate(request).await
    }

    /// Sends a streaming generation request and returns an [`LlmStream`] of events.
    ///
    /// # Errors
    ///
    /// Returns [`GenerationError`] if the provider does not support streaming
    /// or if the request fails.
    pub async fn stream(self) -> Result<LlmStream, GenerationError> {
        let (llm, request) = self.build();
        llm.stream(request).await
    }

    /// Sends the request and extracts a typed value from the response.
    ///
    /// Automatically injects the JSON schema for `T` into the request
    /// and parses the response text into the specified type.
    ///
    /// # Errors
    ///
    /// Returns [`ExtractionError`] if generation fails, no text content
    /// is found, or the response cannot be parsed as type `T`.
    pub async fn generate_structured<T: JsonSchema + DeserializeOwned>(
        self,
    ) -> Result<T, ExtractionError> {
        let (llm, request) = self.build();
        llm.generate_structured::<T>(request).await
    }
}

// ─────────────────────
// Constructor (crate-internal)
// ─────────────────────

impl<'a> LlmRequestBuilder<'a, Empty> {
    /// Creates a new builder for the given LLM.
    #[must_use]
    pub(crate) fn new(llm: &'a Llm) -> Self {
        Self {
            llm,
            tools: Vec::new(),
            system: None,
            messages: Vec::new(),
            tool_choice: None,
            _state: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{AssistantBlock, StopReason, Usage, UserBlock};

    struct MockProvider;

    impl crate::llm::provider::LlmProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }

        async fn generate(
            &self,
            _model: &str,
            request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            let text = request.system.unwrap_or_else(|| "no-system".to_string());
            Ok(LlmResponse {
                content: vec![AssistantBlock::Text(text.into())],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    fn mock_llm() -> Llm {
        let mut registry = crate::ModelRegistry::new();
        registry.register_llm_provider(MockProvider);
        registry.llm("mock/test").unwrap()
    }

    fn make_tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("A {name} tool"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn builder_accumulates_definitions() {
        let llm = mock_llm();
        let builder = llm
            .builder()
            .with_definitions(vec![make_tool_def("a")])
            .with_definitions(vec![make_tool_def("b"), make_tool_def("c")]);

        assert_eq!(builder.tool_count(), 3);
    }

    #[tokio::test]
    async fn send_passes_tools_and_system() {
        struct MockProvider;

        impl crate::llm::provider::LlmProvider for MockProvider {
            fn name(&self) -> &'static str {
                "mock"
            }

            async fn generate(
                &self,
                _model: &str,
                request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                assert_eq!(request.system.as_deref(), Some("Be helpful"));
                assert!(request.tools.is_some());
                let tools = request.tools.as_ref().unwrap();
                assert_eq!(tools.len(), 2);
                assert_eq!(tools[0].name, "search");
                assert_eq!(tools[1].name, "calc");
                assert_eq!(request.messages.len(), 1);

                Ok(LlmResponse {
                    content: vec![AssistantBlock::Text("ok".into())],
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                })
            }
        }

        let mut registry = crate::ModelRegistry::new();
        registry.register_llm_provider(MockProvider);
        let llm = registry.llm("mock/test").unwrap();
        let response = llm
            .builder()
            .with_definitions(vec![make_tool_def("search"), make_tool_def("calc")])
            .system("Be helpful")
            .user("What's the weather?")
            .generate()
            .await
            .unwrap();

        assert_eq!(response.text(), "ok");
    }

    #[tokio::test]
    async fn send_without_tools_sets_none() {
        struct MockProvider;

        impl crate::llm::provider::LlmProvider for MockProvider {
            fn name(&self) -> &'static str {
                "mock"
            }

            async fn generate(
                &self,
                _model: &str,
                request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                assert!(request.tools.is_none());

                Ok(LlmResponse {
                    content: vec![AssistantBlock::Text("hello".into())],
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                })
            }
        }

        let mut registry = crate::ModelRegistry::new();
        registry.register_llm_provider(MockProvider);
        let llm = registry.llm("mock/test").unwrap();
        let response = llm.builder().user("Hello").generate().await.unwrap();

        assert_eq!(response.text(), "hello");
    }

    // ── Streaming tests ──

    mod stream_helpers {
        use crate::llm::error::GenerationError;
        use crate::llm::types::StreamEvent;
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        /// A synchronous stream built from a `Vec` of items for testing.
        pub struct EventStream(pub Vec<Result<StreamEvent, GenerationError>>);

        impl Stream for EventStream {
            type Item = Result<StreamEvent, GenerationError>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.0.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(self.0.remove(0)))
                }
            }
        }
    }

    #[tokio::test]
    async fn stream_default_returns_unsupported() {
        // MockProvider (module-level) does NOT override stream(),
        // so the default returns UnsupportedOperation.
        let llm = mock_llm();
        let result = llm.builder().user("Hello").stream().await;

        assert!(
            matches!(result, Err(GenerationError::UnsupportedOperation(_))),
            "expected UnsupportedOperation error"
        );
    }

    #[tokio::test]
    async fn stream_with_mock_provider() {
        use crate::llm::collector::StreamEventExt;
        use crate::llm::types::{ContentBlockDelta, ContentBlockStartData, StreamEvent, Usage};

        struct StreamingProvider;

        impl crate::llm::provider::LlmProvider for StreamingProvider {
            fn name(&self) -> &'static str {
                "streaming_mock"
            }

            async fn generate(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                Err(GenerationError::UnsupportedOperation("use stream()".into()))
            }

            async fn stream(
                &self,
                _model: &str,
                request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                // Echo the first user message back as a streaming text block.
                let user_text = request
                    .messages
                    .first()
                    .map(|m| match m {
                        Message::User { content } => content
                            .first()
                            .map(|b| match b {
                                UserBlock::Text(t) => t.text.clone(),
                                _ => String::new(),
                            })
                            .unwrap_or_default(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();

                let events = vec![
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    }),
                    Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text(user_text),
                    }),
                    Ok(StreamEvent::ContentBlockStop { index: 0 }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                        },
                    }),
                ];

                Ok(Box::pin(stream_helpers::EventStream(events)))
            }
        }

        let mut registry = crate::ModelRegistry::new();
        registry.register_llm_provider(StreamingProvider);
        let llm = registry.llm("streaming_mock/test").unwrap();

        // Test Llm::stream() directly
        let request = LlmRequest {
            system: None,
            messages: vec![Message::user("Hello stream")],
            tools: None,
            tool_choice: None,
            output_schema: None,
        };
        let stream = llm.stream(request).await.unwrap();
        let response = stream.collect_response().await.unwrap();
        assert_eq!(response.text(), "Hello stream");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, Some(10));

        // Test LlmRequestBuilder::stream()
        let stream = llm
            .builder()
            .system("Be brief")
            .user("Builder stream")
            .stream()
            .await
            .unwrap();
        let response = stream.collect_response().await.unwrap();
        assert_eq!(response.text(), "Builder stream");
    }

    #[tokio::test]
    async fn stream_passes_request_fields() {
        use crate::llm::types::{ContentBlockDelta, ContentBlockStartData, StreamEvent, Usage};

        struct AssertingStreamProvider;

        impl crate::llm::provider::LlmProvider for AssertingStreamProvider {
            fn name(&self) -> &'static str {
                "asserting_stream"
            }

            async fn generate(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                unreachable!("should call stream(), not generate()");
            }

            async fn stream(
                &self,
                model: &str,
                request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                // Verify request fields are passed through correctly
                assert_eq!(model, "test");
                assert_eq!(request.system.as_deref(), Some("Test system"));
                assert!(request.tools.is_some());
                assert_eq!(request.tools.as_ref().unwrap().len(), 1);
                assert_eq!(request.messages.len(), 1);

                let events = vec![
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    }),
                    Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text("ok".into()),
                    }),
                    Ok(StreamEvent::ContentBlockStop { index: 0 }),
                    Ok(StreamEvent::MessageStop {
                        stop_reason: StopReason::EndTurn,
                        usage: Usage::default(),
                    }),
                ];

                Ok(Box::pin(stream_helpers::EventStream(events)))
            }
        }

        let mut registry = crate::ModelRegistry::new();
        registry.register_llm_provider(AssertingStreamProvider);
        let llm = registry.llm("asserting_stream/test").unwrap();

        let stream = llm
            .builder()
            .system("Test system")
            .with_definitions(vec![make_tool_def("search")])
            .user("Hello")
            .stream()
            .await
            .unwrap();

        use crate::llm::collector::StreamEventExt;
        let response = stream.collect_response().await.unwrap();
        assert_eq!(response.text(), "ok");
    }
}
