//! The [`LlmProvider`] trait for LLM model providers.

use super::error::GenerationError;
use super::types::{LlmRequest, LlmResponse, LlmStream};
use std::future::Future;
use std::pin::Pin;

/// Per-million-token USD rates for one model.
///
/// Returned by [`LlmProvider::pricing`] so an estimated cost can be derived
/// from the token counts a provider reports. Rates are list prices baked
/// into the provider implementation and may drift from the provider's
/// current rate card — verify before relying on the figure for billing.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ModelPricing {
    /// USD charged per million full-price (uncached) input tokens.
    pub input_per_million_usd: f64,
    /// USD charged per million output tokens.
    pub output_per_million_usd: f64,
    /// USD charged per million tokens served from the prompt cache.
    pub cache_read_per_million_usd: f64,
    /// USD charged per million tokens written to the prompt cache.
    pub cache_write_per_million_usd: f64,
}

/// Cache-read rate as a fraction of the base input rate (Anthropic 5-minute
/// ephemeral: a cache hit costs 0.1× a fresh input token).
const DEFAULT_CACHE_READ_MULTIPLIER: f64 = 0.1;
/// Cache-write rate as a fraction of the base input rate (Anthropic 5-minute
/// ephemeral: writing the cache costs 1.25× a fresh input token).
const DEFAULT_CACHE_WRITE_MULTIPLIER: f64 = 1.25;

impl ModelPricing {
    /// Creates a pricing record from per-million-token USD rates.
    ///
    /// Cache-tier rates default to the Anthropic 5-minute-ephemeral ratios
    /// (0.1× read, 1.25× write of the input rate); override them with
    /// [`with_cache_rates`](Self::with_cache_rates) for providers that price
    /// caching differently.
    ///
    /// `ModelPricing` is `#[non_exhaustive]` so future rate tiers (batch, …)
    /// can be added without breaking callers — construct it through this
    /// constructor rather than a struct literal.
    #[must_use]
    pub const fn new(input_per_million_usd: f64, output_per_million_usd: f64) -> Self {
        Self {
            input_per_million_usd,
            output_per_million_usd,
            cache_read_per_million_usd: input_per_million_usd * DEFAULT_CACHE_READ_MULTIPLIER,
            cache_write_per_million_usd: input_per_million_usd * DEFAULT_CACHE_WRITE_MULTIPLIER,
        }
    }

    /// Overrides the cache-tier rates (USD per million cache-read / cache-write
    /// tokens) for providers whose caching is priced off the default ratios.
    #[must_use]
    pub const fn with_cache_rates(
        mut self,
        cache_read_per_million_usd: f64,
        cache_write_per_million_usd: f64,
    ) -> Self {
        self.cache_read_per_million_usd = cache_read_per_million_usd;
        self.cache_write_per_million_usd = cache_write_per_million_usd;
        self
    }

    /// Estimated USD cost for a single call given its full-price token counts.
    ///
    /// Ignores cache tiers — use [`cost_with_cache`](Self::cost_with_cache) when
    /// cache-read / cache-write token counts are available.
    #[must_use]
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        self.cost_with_cache(input_tokens, output_tokens, 0, 0)
    }

    /// Estimated USD cost for a single call, billing cached input at the
    /// cache-read / cache-write tiers and the rest at the full input rate.
    #[must_use]
    pub fn cost_with_cache(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> f64 {
        let input = input_tokens as f64 * self.input_per_million_usd;
        let output = output_tokens as f64 * self.output_per_million_usd;
        let cache_read = cache_read_tokens as f64 * self.cache_read_per_million_usd;
        let cache_write = cache_creation_tokens as f64 * self.cache_write_per_million_usd;
        (input + output + cache_read + cache_write) / 1_000_000.0
    }
}

/// Trait implemented by LLM providers for text generation.
///
/// Provider plugins implement this trait to handle generation requests.
///
/// # Examples
///
/// ```no_run
/// use polaris_models::llm::{LlmProvider, LlmRequest, LlmResponse, GenerationError};
///
/// struct MyProvider;
///
/// impl LlmProvider for MyProvider {
///     fn name(&self) -> &'static str { "my_provider" }
///
///     async fn generate(
///         &self,
///         model: &str,
///         request: LlmRequest,
///     ) -> Result<LlmResponse, GenerationError> {
///         // Call your backend here
///         todo!()
///     }
/// }
/// ```
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

    /// Returns per-million-token USD pricing for `model`, when known.
    ///
    /// Consumed to derive an estimated cost from reported token usage (e.g.
    /// the `gen_ai.usage.cost_usd` tracing attribute). The default returns
    /// `None`; providers with a published rate card should override it.
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        None
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
///     DynLlmProvider, LlmRequest, LlmResponse, LlmStream, GenerationError, ModelPricing,
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
///
///     fn pricing(&self, model: &str) -> Option<ModelPricing> {
///         self.inner.pricing(model)
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

    /// Returns per-million-token USD pricing for `model`, when known.
    fn pricing(&self, model: &str) -> Option<ModelPricing>;
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

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        LlmProvider::pricing(self, model)
    }
}

#[cfg(test)]
mod tests {
    use super::ModelPricing;

    #[test]
    fn cost_multiplies_tokens_by_per_million_rate() {
        let pricing = ModelPricing::new(3.0, 15.0);
        // 1M input * $3/M + 500k output * $15/M = $3 + $7.5 = $10.5.
        let cost = pricing.cost(1_000_000, 500_000);
        assert!((cost - 10.5).abs() < 1e-9, "expected $10.50, got {cost}");
    }

    #[test]
    fn cost_is_zero_for_zero_tokens() {
        let pricing = ModelPricing::new(15.0, 75.0);
        assert!(pricing.cost(0, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn with_cache_rates_overrides_the_default_ratios() {
        // `new` derives cache tiers from the input rate (0.1× / 1.25×);
        // `with_cache_rates` replaces them outright.
        let pricing = ModelPricing::new(3.0, 15.0).with_cache_rates(0.5, 6.0);
        // 1M cache-read tokens at the overridden $0.50/M rate.
        let read = pricing.cost_with_cache(0, 0, 1_000_000, 0);
        assert!((read - 0.5).abs() < 1e-9, "cache read = $0.50, got {read}");
        // 1M cache-write tokens at the overridden $6.00/M rate.
        let write = pricing.cost_with_cache(0, 0, 0, 1_000_000);
        assert!(
            (write - 6.0).abs() < 1e-9,
            "cache write = $6.00, got {write}"
        );
    }
}
