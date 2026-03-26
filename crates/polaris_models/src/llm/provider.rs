//! The [`LlmProvider`] trait for LLM model providers.

use super::error::GenerationError;
use super::types::{LlmRequest, LlmResponse, LlmStream};
use std::future::Future;
use std::pin::Pin;

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

/// Object-safe version of [`LlmProvider`].
///
/// This trait boxes the futures returned by [`LlmProvider`] so that providers
/// can be stored as `dyn DynLlmProvider`. A blanket implementation is provided
/// for all `T: LlmProvider`.
///
/// ## When to implement each trait
///
/// - **Implement [`LlmProvider`]** when creating an actual LLM backend (e.g., an
///   `OpenAI` or `Anthropic` adapter). The framework provides the `DynLlmProvider`
///   blanket impl automatically.
/// - **Implement `DynLlmProvider` directly** only when building decorator/wrapper
///   types that hold an `Arc<dyn DynLlmProvider>` and cannot use the
///   `impl Future` return types on [`LlmProvider`].
///
/// # Examples
///
/// A decorator that logs before delegating to the inner provider:
///
/// ```no_run
/// use std::pin::Pin;
/// use std::future::Future;
/// use std::sync::Arc;
/// use polaris_models::llm::{
///     DynLlmProvider, LlmRequest, LlmResponse, LlmStream, GenerationError,
/// };
/// use tracing::info;
///
/// struct LoggingProvider {
///     inner: Arc<dyn DynLlmProvider>,
/// }
///
/// impl DynLlmProvider for LoggingProvider {
///     fn name(&self) -> &'static str { self.inner.name() }
///
///     fn generate<'a>(
///         &'a self,
///         model: &'a str,
///         request: LlmRequest,
///     ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, GenerationError>> + Send + 'a>> {
///         info!("generating with {model}...");
///         self.inner.generate(model, request)
///     }
///
///     fn stream<'a>(
///         &'a self,
///         model: &'a str,
///         request: LlmRequest,
///     ) -> Pin<Box<dyn Future<Output = Result<LlmStream, GenerationError>> + Send + 'a>> {
///         self.inner.stream(model, request)
///     }
/// }
/// ```
pub trait DynLlmProvider: Send + Sync + 'static {
    /// Returns the provider name.
    fn name(&self) -> &'static str;

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

impl<T: LlmProvider> DynLlmProvider for T {
    fn name(&self) -> &'static str {
        LlmProvider::name(self)
    }

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
