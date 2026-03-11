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
use super::types::{LlmRequest, LlmResponse, Message, ToolChoice, ToolDefinition};
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
