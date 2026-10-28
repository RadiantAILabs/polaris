//! Anthropic provider backend.
//!
//! Uses the Anthropic messages API.
//!
//! ```no_run
//! # use polaris_model_providers::anthropic::AnthropicPlugin;
//! # use polaris_system::server::Server;
//! # let mut server = Server::new();
//!
//! server.add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"));
//! ```

mod client;
mod plugin;
mod provider;
mod types;

pub use plugin::AnthropicPlugin;
pub use provider::{AnthropicProvider, DEFAULT_MAX_TOKENS};
