//! The [`LlmProvider`] trait for LLM model providers.

use std::future::Future;
use std::pin::Pin;

use super::error::GenerationError;
use super::types::{LlmRequest, LlmResponse, LlmStream};

/// Trait implemented by LLM providers for text generation.
///
/// Provider plugins implement this trait to handle generation requests.
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
    fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> impl Future<Output = Result<LlmResponse, GenerationError>> + Send;

    /// Sends a streaming generation request to the provider.
    ///
    /// Returns an [`LlmStream`] of incremental [`StreamEvent`](super::types::StreamEvent) events.
    /// The default implementation returns [`GenerationError::UnsupportedOperation`].
    ///
    /// # Arguments
    ///
    /// * `model` - The model name on which to perform generation
    /// * `request` - The generation request
    fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> impl Future<Output = Result<LlmStream, GenerationError>> + Send {
        async {
            Err(GenerationError::UnsupportedOperation(
                "streaming not supported".to_owned(),
            ))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Erased trait for object safety
// ─────────────────────────────────────────────────────────────────────────────

/// Type-erased provider trait for object safety.
pub(crate) trait ErasedLlmProvider: Send + Sync + 'static {
    /// Sends a generation request.
    fn generate<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>>;

    /// Sends a streaming generation request.
    fn stream<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>>;
}

impl<T: LlmProvider> ErasedLlmProvider for T {
    fn generate<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>> {
        Box::pin(self.generate(model, request))
    }

    fn stream<'a>(
        &'a self,
        model: &'a str,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
        Box::pin(self.stream(model, request))
    }
}
