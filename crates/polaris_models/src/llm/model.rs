//! LLM handle for generation requests.

use super::builder::LlmRequestBuilder;
use super::error::{ExtractionError, GenerationError};
use super::provider::LlmProvider;
use super::types::{LlmRequest, LlmResponse};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// An LLM handle for making generation requests.
///
/// Created via [`ModelRegistry::llm()`](crate::ModelRegistry::llm).
#[derive(Clone)]
pub struct Llm {
    provider: Arc<dyn LlmProvider>,
    model: String,
}

impl Llm {
    /// Creates a new LLM handle from a provider and model name.
    #[must_use]
    pub(crate) fn new(provider: Arc<dyn LlmProvider>, model: String) -> Self {
        Self { provider, model }
    }

    /// Sends a generation request to the model.
    ///
    /// # Errors
    ///
    /// Returns a [`GenerationError`] if the request fails.
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

    /// Creates a builder for a single-shot LLM request.
    ///
    /// The builder accumulates messages, tool definitions, and options,
    /// then sends a single generation request via [`LlmRequestBuilder::generate()`].
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
