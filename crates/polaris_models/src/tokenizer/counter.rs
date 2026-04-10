//! Token counting trait and types.

use crate::llm::{LlmRequest, Message};
use std::fmt;

/// The result of counting tokens.
///
/// The [`exact`](Self::exact) flag distinguishes counts produced by a known
/// BPE tokenizer (`true`) from character-based heuristic estimates (`false`).
/// Consumers can use this to decide whether to apply safety margins.
///
/// # Accuracy
///
/// `exact = true` means the count was produced by a real BPE tokenizer, **not**
/// that it perfectly matches the provider's internal tokenizer. For example,
/// Anthropic models are counted using the `cl100k_base` encoding, which is a
/// close approximation but not identical to Claude's actual tokenizer. `OpenAI`
/// models mapped to their native encoding family (e.g. `o200k_base` for GPT-5.4)
/// will be truly exact.
///
/// When `exact = false`, the count is a rough `ceil(chars / 4)` heuristic.
/// BPE-based counts (even approximate) are significantly more accurate than
/// heuristic estimates.
///
/// # Examples
///
/// ```
/// use polaris_models::tokenizer::TokenCount;
///
/// let count = TokenCount::exact(42);
/// assert_eq!(count.tokens, 42);
/// assert!(count.exact);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct TokenCount {
    /// Number of tokens.
    pub tokens: usize,
    /// Whether the count was produced by an exact tokenizer (`true`) or a
    /// heuristic estimate (`false`).
    pub exact: bool,
}

impl TokenCount {
    /// Creates an exact token count.
    #[must_use]
    pub fn exact(tokens: usize) -> Self {
        Self {
            tokens,
            exact: true,
        }
    }

    /// Creates an estimated (non-exact) token count.
    #[must_use]
    pub fn estimate(tokens: usize) -> Self {
        Self {
            tokens,
            exact: false,
        }
    }
}

impl fmt::Display for TokenCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exact {
            write!(f, "{} tokens", self.tokens)
        } else {
            write!(f, "~{} tokens", self.tokens)
        }
    }
}

/// Errors that can occur during token counting.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TokenCountError {
    /// The tokenizer could not encode the provided input.
    #[error("encoding failed: {0}")]
    EncodingFailed(String),
}

/// Counts tokens in text, messages, and requests for a given model.
///
/// Implementations map model names to tokenizers. When an exact tokenizer is
/// available the returned [`TokenCount`] has `exact = true`; otherwise a
/// heuristic estimate is returned with `exact = false`.
///
/// # Design
///
/// `TokenCounter` is object-safe and can be stored as `Arc<dyn TokenCounter>`.
/// It is intentionally synchronous — tokenization is CPU-bound and fast — so no
/// async / boxed-future machinery is needed.
///
/// # Stability
///
/// This trait is open for downstream implementation. Adding required methods
/// in the future is a **breaking change**. New methods will always be added
/// with default implementations to preserve backward compatibility.
///
/// # Examples
///
/// ```
/// use polaris_models::tokenizer::{TokenCount, TokenCountError, TokenCounter};
/// use polaris_models::llm::{LlmRequest, Message};
///
/// /// A counter that uses a fixed 4-chars-per-token heuristic.
/// struct HeuristicCounter;
///
/// impl TokenCounter for HeuristicCounter {
///     fn count_text(&self, _model: &str, text: &str) -> Result<TokenCount, TokenCountError> {
///         Ok(TokenCount::estimate(text.chars().count().div_ceil(4)))
///     }
///     fn count_message(&self, model: &str, msg: &Message) -> Result<TokenCount, TokenCountError> {
///         self.count_text(model, "placeholder")
///     }
///     fn count_messages(&self, model: &str, msgs: &[Message]) -> Result<TokenCount, TokenCountError> {
///         self.count_text(model, "placeholder")
///     }
///     fn count_request(&self, model: &str, req: &LlmRequest) -> Result<TokenCount, TokenCountError> {
///         self.count_text(model, "placeholder")
///     }
/// }
///
/// let counter = HeuristicCounter;
/// let count = counter.count_text("any-model", "Hello, world!")?;
/// assert_eq!(count.tokens, 4); // ceil(13/4)
/// assert!(!count.exact);
/// # Ok::<(), TokenCountError>(())
/// ```
pub trait TokenCounter: Send + Sync {
    /// Counts tokens in a plain text string.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode the input.
    fn count_text(&self, model: &str, text: &str) -> Result<TokenCount, TokenCountError>;

    /// Counts tokens in a single message, including per-message overhead.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode the message content.
    fn count_message(&self, model: &str, message: &Message) -> Result<TokenCount, TokenCountError>;

    /// Counts tokens in a slice of messages, including conversation overhead.
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode any message.
    fn count_messages(
        &self,
        model: &str,
        messages: &[Message],
    ) -> Result<TokenCount, TokenCountError>;

    /// Counts tokens in a full LLM request (system prompt, messages, and tool
    /// definitions).
    ///
    /// # Errors
    ///
    /// Returns [`TokenCountError::EncodingFailed`] if the underlying tokenizer
    /// fails to encode any part of the request.
    fn count_request(
        &self,
        model: &str,
        request: &LlmRequest,
    ) -> Result<TokenCount, TokenCountError>;
}
