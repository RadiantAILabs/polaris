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
//! It is separate from `OpenAiPlugin`, which
//! speaks the Responses API, so one server can register both and each model id
//! says which protocol it uses.
//!
//! Requests and responses are handled as raw JSON, not through typed models,
//! because servers differ in which fields they send. See
//! [`ChatCompletionsProvider`] for what is read and what is sent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

mod plugin;
mod provider;
mod request;
mod response;
mod stream;

pub use plugin::ChatCompletionsPlugin;
pub use provider::{ChatCompletionsProvider, Timeouts};

/// Which request key carries [`LlmRequest::max_output_tokens`](polaris_models::llm::LlmRequest::max_output_tokens).
///
/// `OpenAI` replaced `max_tokens` with `max_completion_tokens`, and rejects
/// `max_tokens` for its reasoning models. Many other servers still read only
/// `max_tokens`, or do not enforce `max_completion_tokens`; the Hugging Face
/// router's request schema lists only `max_tokens`. Pick the key the server
/// honours with [`ChatCompletionsProvider::with_max_tokens_field`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MaxTokensField {
    /// Send `max_completion_tokens` (the default, right for `OpenAI`).
    #[default]
    MaxCompletionTokens,
    /// Send `max_tokens` (use this for the Hugging Face router and for
    /// servers that read only that key).
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
/// the unknown field. Which name a server reads on input is a fact about that
/// server, so the default sends the reasoning under the name the server itself
/// used. Set another value with
/// [`ChatCompletionsProvider::with_reasoning_replay`] when a server answers
/// under one name and reads another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReasoningReplay {
    /// Send the reasoning under the field the server used in its latest reply
    /// for the same model through this provider (the default). Before that model
    /// has returned any reasoning it uses `reasoning_content`; that only matters
    /// for history carried over from another process or provider.
    #[default]
    AsReceived,
    /// Always send the reasoning as the assistant message's `reasoning_content`
    /// field (`DeepSeek`, Z.ai and `SGLang` read this name).
    ReasoningContent,
    /// Always send the reasoning as the assistant message's `reasoning` field.
    Reasoning,
    /// Do not send earlier reasoning.
    Omit,
}

const REASONING_CONTENT: &str = "reasoning_content";
const REASONING: &str = "reasoning";

impl ReasoningReplay {
    /// The assistant message field that carries the reasoning, if any.
    pub(crate) fn field(self, seen: &SeenReasoningField) -> Option<&'static str> {
        match self {
            Self::AsReceived => Some(seen.get()),
            Self::ReasoningContent => Some(REASONING_CONTENT),
            Self::Reasoning => Some(REASONING),
            Self::Omit => None,
        }
    }
}

/// A per-model handle to reasoning fields shared by every clone of a provider.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeenReasoningField {
    models: Arc<Mutex<HashMap<String, bool>>>,
    model: String,
}

impl SeenReasoningField {
    /// Selects a model while sharing the provider's records.
    pub(crate) fn for_model(&self, model: &str) -> Self {
        Self {
            models: Arc::clone(&self.models),
            model: model.to_owned(),
        }
    }

    /// Records the field a reply used. Other names are ignored.
    pub(crate) fn record(&self, field: &str) {
        let reasoning = match field {
            REASONING => true,
            REASONING_CONTENT => false,
            _ => return,
        };
        self.models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.model.clone(), reasoning);
    }

    fn get(&self) -> &'static str {
        if self
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.model)
            .copied()
            .unwrap_or(false)
        {
            REASONING
        } else {
            REASONING_CONTENT
        }
    }
}

#[cfg(test)]
mod server_tests;
