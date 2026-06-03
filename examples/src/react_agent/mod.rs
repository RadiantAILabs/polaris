//! ReAct agent definition and core types.
//!
//! # Crate Structure
//!
//! - `agent` — ReAct agent graph definition and systems
//! - `config` — Agent configuration
//! - `context` — Conversation history management
//! - `state` — Agent loop state tracking

mod agent;
mod config;
mod context;
mod state;

pub use agent::{ReActAgent, ReActPlugin};
pub use config::AgentConfig;
pub use context::ContextManager;
pub use state::ReactState;
