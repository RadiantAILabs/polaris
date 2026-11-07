//! `OpenAI` provider backend.
//!
//! Uses the `OpenAI` Responses API.

mod compat;
mod plugin;
pub(crate) mod provider;

pub use plugin::OpenAiPlugin;
pub use provider::OpenAiProvider;
