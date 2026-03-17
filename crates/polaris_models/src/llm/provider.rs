//! The [`LlmProvider`] trait for LLM model providers.

use super::error::GenerationError;
use super::types::{LlmRequest, LlmResponse};
use async_trait::async_trait;

/// Trait implemented by LLM providers for text generation.
///
/// Provider plugins implement this trait to handle generation requests.
#[async_trait]
pub trait LlmProvider: Send + Sync + 'static {
    /// Returns the provider name (e.g., `"openai"`, `"anthropic"`).
    ///
    /// This name is used as the registry key.
    fn name(&self) -> &'static str;

    /// Sends a generation request to the provider.
    ///
    /// # Arguments
    ///
    /// * `model` - The model name on which to perform generation
    /// * `request` - The generation request
    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError>;
}
