//! LLM handle for generation requests.

use super::builder::LlmRequestBuilder;
use super::error::{ExtractionError, GenerationError};
use super::provider::DynLlmProvider;
use super::types::{LlmRequest, LlmResponse, LlmStream};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// An LLM handle for making generation requests.
///
/// Created via [`ModelRegistry::llm()`](crate::ModelRegistry::llm).
#[derive(Clone)]
pub struct Llm {
    provider: Arc<dyn DynLlmProvider>,
    model: String,
}

impl Llm {
    /// Creates a new LLM handle from a provider and model name.
    #[must_use]
    pub(crate) fn new(provider: Arc<dyn DynLlmProvider>, model: String) -> Self {
        Self { provider, model }
    }

    /// Sends a generation request to the model.
    ///
    /// # Errors
    ///
    /// Returns a [`GenerationError`] if the request fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_models::{ModelRegistry, llm::{LlmRequest, Message}};
    /// # async fn example(registry: &ModelRegistry) -> Result<(), Box<dyn std::error::Error>> {
    /// let llm = registry.llm("anthropic/claude-sonnet-4-20250514")?;
    /// let request = LlmRequest {
    ///     messages: vec![Message::user("Hello!")],
    ///     ..Default::default()
    /// };
    /// let response = llm.generate(request).await?;
    /// let text = response.text();
    /// # Ok(())
    /// # }
    /// ```
    pub async fn generate(&self, request: LlmRequest) -> Result<LlmResponse, GenerationError> {
        self.provider.generate(&self.model, request).await
    }

    /// Sends a generation request with structured output.
    ///
    /// This method automatically injects the JSON schema for type `T` into the request
    /// and parses the response into the specified type.
    ///
    /// # Errors
    ///
    /// Returns an [`ExtractionError`] if:
    /// - The generation request fails
    /// - No text content is found in the response
    /// - The response cannot be parsed as type `T`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_models::{ModelRegistry, llm::{LlmRequest, Message}};
    /// # use schemars::JsonSchema;
    /// # use serde::Deserialize;
    /// #[derive(Deserialize, JsonSchema)]
    /// struct Sentiment { score: f64, label: String }
    ///
    /// # async fn example(registry: &ModelRegistry) -> Result<(), Box<dyn std::error::Error>> {
    /// let llm = registry.llm("anthropic/claude-sonnet-4-20250514")?;
    /// let request = LlmRequest {
    ///     messages: vec![Message::user("Analyze: 'I love this!'")],
    ///     ..Default::default()
    /// };
    /// let result: Sentiment = llm.generate_structured(request).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn generate_structured<T: JsonSchema + DeserializeOwned>(
        &self,
        mut request: LlmRequest,
    ) -> Result<T, ExtractionError> {
        // Inject schema into request
        let schema = schema_for!(T);
        request.output_schema = Some(
            serde_json::to_value(schema)
                .map_err(|err| ExtractionError::SchemaSerializationError(err.to_string()))?,
        );

        // Generate response
        let response = self.generate(request).await?;

        // Extract text content
        let text = response.text();
        if text.is_empty() {
            return Err(ExtractionError::NoContent);
        }

        // Parse as structured data
        Ok(serde_json::from_str(&text)?)
    }

    /// Sends a streaming generation request to the model.
    ///
    /// Returns an [`LlmStream`] of incremental [`StreamEvent`](super::types::StreamEvent) events.
    ///
    /// # Errors
    ///
    /// Returns a [`GenerationError`] if the provider does not support streaming
    /// or if the request fails.
    pub async fn stream(&self, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.provider.stream(&self.model, request).await
    }

    /// Creates a builder for a single-shot LLM request.
    ///
    /// The builder accumulates messages, tool definitions, and options,
    /// then sends a single generation request via [`LlmRequestBuilder::generate()`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use polaris_models::{ModelRegistry, llm::Message};
    /// # async fn example(registry: &ModelRegistry) -> Result<(), Box<dyn std::error::Error>> {
    /// let llm = registry.llm("openai/gpt-4o")?;
    /// let response = llm.builder()
    ///     .system("You are a helpful assistant.")
    ///     .message(Message::user("Hello!"))
    ///     .generate()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn builder(&self) -> LlmRequestBuilder<'_> {
        LlmRequestBuilder::new(self)
    }

    /// Returns the model name (without provider prefix).
    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.model
    }
}
