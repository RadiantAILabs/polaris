//! Chat Completions provider backend.
//!
//! Talks to any server that implements the `OpenAI` Chat Completions API,
//! `POST {base_url}/chat/completions`: `OpenAI` itself, the Hugging Face
//! inference router, vLLM, `SGLang`, Ollama, `DeepInfra`, Z.ai, `DeepSeek` and others.
//!
//! The provider is registered under the name `chat_completions`, so models are
//! addressed as `chat_completions/<model>`, where `<model>` is the exact string
//! the server expects in the request's `model` field (it may contain `/` and
//! `:`, for example `zai-org/GLM-5.3-Flash:zai`).
//!
//! It is separate from [`OpenAiPlugin`](crate::openai::OpenAiPlugin), which
//! speaks the Responses API, so one server can register both and each model id
//! says which protocol it uses.
//!
//! Requests and responses are handled as raw JSON, not through typed models,
//! because servers differ in which fields they send. See
//! [`ChatCompletionsProvider`] for what is read and what is sent.

mod plugin;
mod provider;
mod request;
mod response;
mod stream;

pub use plugin::ChatCompletionsPlugin;
pub use provider::ChatCompletionsProvider;

/// Which request key carries [`LlmRequest::max_output_tokens`](polaris_models::llm::LlmRequest::max_output_tokens).
///
/// `OpenAI` replaced `max_tokens` with `max_completion_tokens`, and rejects
/// `max_tokens` for its reasoning models. Many other servers still read only
/// `max_tokens`, or do not enforce `max_completion_tokens`. Pick the key the
/// server honours with
/// [`ChatCompletionsProvider::with_max_tokens_field`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaxTokensField {
    /// Send `max_completion_tokens` (the default).
    #[default]
    MaxCompletionTokens,
    /// Send `max_tokens`.
    MaxTokens,
}

impl MaxTokensField {
    /// The request key.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxTokens => "max_tokens",
        }
    }
}

/// How reasoning from earlier assistant turns is sent back to the server.
///
/// Servers return a model's reasoning next to its answer, in the
/// `reasoning_content` field or, on newer servers, `reasoning`. Some models need
/// it back on later turns (`DeepSeek`'s thinking mode requires it on every
/// request that carries tools and answers `400` without it; Z.ai's preserved
/// thinking needs it complete and in order), while others ignore it or reject
/// the unknown field. Pick what the server expects with
/// [`ChatCompletionsProvider::with_reasoning_replay`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReasoningReplay {
    /// Send the reasoning as the assistant message's `reasoning_content`
    /// field (the default; `DeepSeek` and Z.ai read this name).
    #[default]
    ReasoningContent,
    /// Send the reasoning as the assistant message's `reasoning` field.
    Reasoning,
    /// Do not send earlier reasoning.
    Omit,
}

impl ReasoningReplay {
    /// The assistant message field that carries the reasoning, if any.
    pub(crate) fn field(self) -> Option<&'static str> {
        match self {
            Self::ReasoningContent => Some("reasoning_content"),
            Self::Reasoning => Some("reasoning"),
            Self::Omit => None,
        }
    }
}

#[cfg(test)]
mod server_tests;
