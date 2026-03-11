//! Error types for LLM generation operations.

use std::time::Duration;

/// Errors for structured output extraction.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    /// No content found in response to extract.
    #[error("no content found in response")]
    NoContent,

    /// Failed to deserialize the extracted data.
    #[error("failed to deserialize extracted data: {0}")]
    Deserialization(#[from] serde_json::Error),

    /// Failed to serialize the schema.
    #[error("failed to serialize schema: {0}")]
    SchemaSerializationError(String),

    /// Underlying generation request failed.
    #[error("generation failed: {0}")]
    GenerationError(#[from] GenerationError),
}

/// Errors for LLM generation operations.
#[derive(Debug, thiserror::Error)]
pub enum GenerationError {
    /// Http error (e.g.: connection error, timeout, etc.)
    #[error("http error: {0}")]
    Http(String),

    /// JSON serialization/deserialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Authentication failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Rate limited by the provider.
    #[error("rate limited{}", .retry_after.map(|d| format!(", retry after {d:?}")).unwrap_or_default())]
    RateLimited {
        /// Suggested time to wait before retrying.
        retry_after: Option<Duration>,
    },

    /// Error parsing the request.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Error parsing the response.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Request contains content that the provider does not support.
    #[error("unsupported content: {0}")]
    UnsupportedContent(String),

    /// The model refused to fulfill the request (e.g. content policy).
    #[error("model refused the request: {0}")]
    Refusal(String),

    /// Error returned by the model provider.
    #[error("provider error: {message}")]
    Provider {
        /// HTTP status code if available.
        status: Option<u16>,
        /// Error message.
        message: String,
        /// The underlying error source.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}
