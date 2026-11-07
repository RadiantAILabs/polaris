#![cfg_attr(docsrs_dep, feature(doc_cfg))]

//! Plugins providing support for various model provider backends.
//!
//! Each provider is packaged as a standalone plugin. When added to the server, they register themselves with the [`ModelRegistry`](polaris_models::ModelRegistry), allowing standardized access to different model providers.
//!
//! # Supported Providers
//!
//! | Provider | Feature Flag | Description |
//! |----------|--------------|-------------|
//! | Anthropic | `anthropic` (default) | Direct Anthropic API access |
//! | `OpenAI` | `openai` | `OpenAI` Responses API |
//! | Chat Completions | `chat-completions` | Any server that implements the `OpenAI` Chat Completions API (Hugging Face router, vLLM, Ollama, `DeepInfra`, Z.ai, `DeepSeek`, `OpenAI`); no extra dependency |
//! | AWS Bedrock | `bedrock` (also enables `openai`) | AWS Bedrock Converse API; `openai.*` models use Bedrock's `OpenAI` Responses endpoint |
//!
//! # Feature Flags
//!
//! Each provider is gated behind a feature flag to avoid pulling in unnecessary dependencies.
//!
//! ```toml
//! # Enable only Anthropic (default)
//! polaris_model_providers = { path = "../polaris_model_providers" }
//!
//! # Enable Bedrock (also exposes `openai` and `OpenAiPlugin`)
//! polaris_model_providers = { path = "../polaris_model_providers", default-features = false, features = ["bedrock"] }
//!
//! # Enable OpenAI
//! polaris_model_providers = { path = "../polaris_model_providers", default-features = false, features = ["openai"] }
//!
//! # Enable only Chat Completions
//! polaris_model_providers = { path = "../polaris_model_providers", default-features = false, features = ["chat-completions"] }
//!
//! # Enable multiple providers
//! polaris_model_providers = { path = "../polaris_model_providers", features = ["openai", "bedrock"] }
//! ```
//!
//! # Usage
//!
//! All provider plugins depend on [`ModelsPlugin`](polaris_models::ModelsPlugin), which must be added first to register the [`ModelRegistry`](polaris_models::ModelRegistry) resource.
//! The Anthropic output token limit is configured at registration time and
//! defaults to 4096 when omitted. An `LlmRequest` may lower that ceiling for
//! one request, but cannot raise it.
//!
//! ```no_run
//! # #[cfg(feature = "anthropic")]
//! # {
//! use core::num::NonZeroU32;
//!
//! use polaris_model_providers::AnthropicPlugin;
//! use polaris_models::ModelsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(ModelsPlugin);
//! server.add_plugins(
//!     AnthropicPlugin::from_env("ANTHROPIC_API_KEY")
//!         .with_max_tokens(NonZeroU32::new(32_000).unwrap()),
//! );
//! # }
//! ```
//!
//! For AWS Bedrock, credentials are loaded from the default AWS credential chain:
//!
//! ```no_run
//! # #[cfg(feature = "bedrock")]
//! # {
//! use polaris_model_providers::BedrockPlugin;
//! use polaris_models::ModelsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(ModelsPlugin);
//! server.add_plugins(BedrockPlugin::from_env());
//! # }
//! ```
//!
//! For `OpenAI`, provide an API key via environment variable:
//!
//! ```no_run
//! # #[cfg(feature = "openai")]
//! # {
//! use polaris_model_providers::OpenAiPlugin;
//! use polaris_models::ModelsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(ModelsPlugin);
//! server.add_plugins(OpenAiPlugin::from_env("OPENAI_API_KEY"));
//! # }
//! ```
//!
//! For a server that speaks the Chat Completions API, give the server's base URL:
//!
//! ```no_run
//! # #[cfg(feature = "chat-completions")]
//! # {
//! use polaris_model_providers::{ChatCompletionsPlugin, chat_completions::MaxTokensField};
//! use polaris_models::ModelsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(ModelsPlugin);
//! server.add_plugins(
//!     ChatCompletionsPlugin::from_env("HF_TOKEN")
//!         .with_base_url("https://router.huggingface.co/v1")
//!         .with_max_tokens_field(MaxTokensField::MaxTokens),
//! );
//! # }
//! ```
//!
//! # Reasoning Effort
//!
//! [`LlmRequest::reasoning_effort`](polaris_models::llm::LlmRequest::reasoning_effort)
//! asks for a reasoning level on any provider. Chat Completions, `OpenAI`
//! Responses and Bedrock `openai.*` models send it as written, Anthropic sends
//! it as `output_config.effort` (`low` to `max`), and Bedrock Converse does not
//! support it. A provider that cannot send the requested level returns
//! [`GenerationError::UnsupportedOperation`](polaris_models::llm::GenerationError::UnsupportedOperation)
//! before it sends anything.
//!
//! # See Also
//!
//! For the full framework guide, adding custom providers, and integration
//! patterns, see the [`polaris-ai` crate documentation](https://docs.rs/polaris-ai).

mod schema;

pub mod pricing;

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicPlugin;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "openai")]
pub use openai::OpenAiPlugin;

#[cfg(feature = "chat-completions")]
mod sse;

#[cfg(feature = "chat-completions")]
pub mod chat_completions;

#[cfg(feature = "chat-completions")]
pub use chat_completions::ChatCompletionsPlugin;

#[cfg(feature = "bedrock")]
pub mod bedrock;

#[cfg(feature = "bedrock")]
pub use bedrock::BedrockPlugin;
