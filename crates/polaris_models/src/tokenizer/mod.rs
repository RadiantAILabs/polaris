//! Token counting for LLM requests and messages.
//!
//! This module provides the [`TokenCounter`] trait for model-agnostic token
//! counting and a [`TiktokenCounter`] implementation backed by tiktoken BPE
//! tokenizers. Context management and memory chunking both depend on accurate
//! (or conservatively estimated) token counts.
//!
//! # Quick start
//!
//! ```
//! use polaris_models::tokenizer::TokenCount;
//!
//! // Exact count produced by a BPE tokenizer
//! let exact = TokenCount::exact(42);
//! assert_eq!(exact.tokens, 42);
//! assert!(exact.exact);
//!
//! // Heuristic estimate for an unknown model
//! let estimate = TokenCount::estimate(10);
//! assert!(!estimate.exact);
//! ```

use crate::llm::{LlmRequest, Message};
pub use counter::{TokenCount, TokenCountError, TokenCounter};
use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;
use std::sync::Arc;

#[cfg(feature = "tiktoken")]
pub use tiktoken::{EncodingFamily, TiktokenCounter};

mod counter;
#[cfg(feature = "tiktoken")]
mod tiktoken;

// ─────────────────────────────────────────────────────────────────────────────
// Tokenizer resource
// ─────────────────────────────────────────────────────────────────────────────

/// Global token-counting resource.
///
/// Wraps an `Arc<dyn TokenCounter>` and is registered by [`TokenizerPlugin`].
/// Systems access it via `Res<Tokenizer>`.
///
/// # Examples
///
/// ```
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_models::tokenizer::{Tokenizer, TokenCounter};
///
/// #[system]
/// async fn check_budget(tokenizer: Res<Tokenizer>) {
///     if let Ok(count) = tokenizer.count_text("claude-opus-4-6", "Hello!") {
///         assert!(count.tokens > 0);
///     }
/// }
/// ```
pub struct Tokenizer {
    counter: Arc<dyn TokenCounter>,
}

impl GlobalResource for Tokenizer {}

impl Tokenizer {
    /// Creates a `Tokenizer` wrapping the given counter.
    pub fn new(counter: Arc<dyn TokenCounter>) -> Self {
        Self { counter }
    }
}

impl TokenCounter for Tokenizer {
    /// Counts tokens in a plain text string.
    ///
    /// Delegates to the wrapped [`TokenCounter`] implementation.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode the input.
    fn count_text(&self, model: &str, text: &str) -> Result<TokenCount, TokenCountError> {
        self.counter.count_text(model, text)
    }

    /// Counts tokens in a single message, including per-message overhead.
    ///
    /// Delegates to the wrapped [`TokenCounter`] implementation.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode the message content.
    fn count_message(&self, model: &str, message: &Message) -> Result<TokenCount, TokenCountError> {
        self.counter.count_message(model, message)
    }

    /// Counts tokens in a slice of messages, including conversation overhead.
    ///
    /// Delegates to the wrapped [`TokenCounter`] implementation.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode any message.
    fn count_messages(
        &self,
        model: &str,
        messages: &[Message],
    ) -> Result<TokenCount, TokenCountError> {
        self.counter.count_messages(model, messages)
    }

    /// Counts tokens in a full LLM request.
    ///
    /// Delegates to the wrapped [`TokenCounter`] implementation.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode any part of the request.
    fn count_request(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<TokenCount, TokenCountError> {
        self.counter.count_request(model, request)
    }
}

impl std::fmt::Debug for Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokenizer").finish_non_exhaustive()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TokenizerPlugin
// ─────────────────────────────────────────────────────────────────────────────

/// Plugin that registers a [`Tokenizer`] global resource.
///
/// With the `tiktoken` feature, [`TokenizerPlugin::default`] creates a
/// [`TiktokenCounter`]-backed tokenizer. Without the `tiktoken` feature,
/// `Default` is **not available** — use [`TokenizerPlugin::new`] to supply a
/// custom [`TokenCounter`] implementation.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`Tokenizer`] | Global | Model-agnostic token counter for text, messages, and requests |
///
/// # Dependencies
///
/// None.
///
/// # Example
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_models::tokenizer::TokenizerPlugin;
///
/// let mut server = Server::new();
/// server.add_plugins(TokenizerPlugin::default());
/// ```
pub struct TokenizerPlugin {
    counter: Arc<dyn TokenCounter>,
}

impl std::fmt::Debug for TokenizerPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenizerPlugin").finish_non_exhaustive()
    }
}

impl TokenizerPlugin {
    /// Creates a plugin with a custom [`TokenCounter`] implementation.
    pub fn new(counter: Arc<dyn TokenCounter>) -> Self {
        Self { counter }
    }
}

#[cfg(feature = "tiktoken")]
impl Default for TokenizerPlugin {
    fn default() -> Self {
        Self {
            counter: Arc::new(TiktokenCounter::new()),
        }
    }
}

impl Plugin for TokenizerPlugin {
    const ID: &'static str = "polaris::tokenizer";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_global(Tokenizer::new(Arc::clone(&self.counter)));
    }

    fn dependencies(&self) -> Vec<polaris_system::plugin::PluginId> {
        vec![]
    }
}
