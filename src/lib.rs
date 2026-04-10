//! A modular framework for building AI agents in Rust.
//!
//! Polaris is an ECS-inspired runtime for composing AI agents as directed
//! graphs of systems. It provides layered abstractions — from low-level
//! dependency-injected systems, through graph-based execution, up to
//! session management and HTTP serving.
//!
//! # Crate Organisation
//!
//! | Module | Crate | Purpose |
//! |--------|-------|---------|
//! | [`system`] | `polaris_system` | ECS-inspired systems, resources, and plugins |
//! | [`graph`] | `polaris_graph` | Directed-graph execution primitives |
//! | [`agent`] | `polaris_agent` | Agent trait for reusable behavior patterns |
//! | [`tools`] | `polaris_tools` | Tool framework for LLM-callable functions |
//! | [`models`] | `polaris_models` / `polaris_model_providers` | Model registry and provider implementations |
//! | [`plugins`] | `polaris_core_plugins` | Core infrastructure plugins (time, tracing, persistence) |
//! | [`sessions`] | `polaris_sessions` | Session management and orchestration |
//! | [`shell`] | `polaris_shell` | Shell command execution with permission model |
//! | [`app`] | `polaris_app` | HTTP server runtime with plugin integration |
//!
//! # Feature Flags
//!
//! All features are opt-in (none enabled by default). Features that originate
//! from a sub-crate and would otherwise be ambiguous at the top level are
//! prefixed with the sub-crate's short name (e.g. `sessions-http`). Features
//! that are already unambiguous keep their original name (e.g. `anthropic`).
//!
//! ## Model Providers
//!
//! | Feature | Enables |
//! |---------|---------|
//! | `anthropic` | Anthropic Claude provider (`polaris_model_providers`) |
//! | `openai` | `OpenAI` provider (`polaris_model_providers`) |
//! | `bedrock` | AWS Bedrock provider (`polaris_model_providers`) |
//!
//! ## Observability
//!
//! | Feature | Enables |
//! |---------|---------|
//! | `graph-tracing` | Tracing spans for graph execution (`polaris_core_plugins`) |
//! | `models-tracing` | Tracing spans for model calls (`polaris_core_plugins`) |
//! | `tools-tracing` | Tracing spans for tool invocations (`polaris_core_plugins`) |
//! | `otel` | OpenTelemetry exporter support (`polaris_core_plugins`) |
//!
//! ## Tokenization
//!
//! | Feature | Enables |
//! |---------|---------|
//! | `tiktoken` | BPE token counting via tiktoken (`polaris_models`) |
//!
//! ## Sessions
//!
//! | Feature | Enables |
//! |---------|---------|
//! | `sessions-http` | HTTP/REST routes for session management (`polaris_sessions`) |
//!
//! # Quick Start
//!
//! ```no_run
//! use polaris_ai::prelude::*;
//! use polaris_ai::system::server::Server;
//!
//! // Build a server with plugins
//! let mut server = Server::new();
//! // Add plugins, agents, and run...
//! ```

// Re-export crates under their original names so proc-macro-generated code
// can resolve `polaris::polaris_tools`, `polaris::polaris_system`, etc.
#[doc(hidden)]
pub use polaris_internal::polaris_core_plugins;
#[doc(hidden)]
pub use polaris_internal::polaris_models;
#[doc(hidden)]
pub use polaris_internal::polaris_system;
#[doc(hidden)]
pub use polaris_internal::polaris_tools;

/// Re-export all common types for easy access.
pub mod prelude {
    pub use polaris_internal::prelude::*;
}

/// ECS-inspired systems, resources, and plugins.
pub mod system {
    #[doc(inline)]
    pub use polaris_internal::system::*;
}

/// Directed-graph execution primitives.
pub mod graph {
    #[doc(inline)]
    pub use polaris_internal::graph::*;
}

/// Agent trait for reusable behavior patterns.
pub mod agent {
    #[doc(inline)]
    pub use polaris_internal::agent::*;
}

/// Tool framework for LLM-callable functions.
pub mod tools {
    #[doc(inline)]
    pub use polaris_internal::tools::*;
}

/// Model registry and provider implementations.
pub mod models {
    #[doc(inline)]
    pub use polaris_internal::models::*;
}

/// Core infrastructure plugins (time, tracing, persistence).
pub mod plugins {
    #[doc(inline)]
    pub use polaris_internal::plugins::*;
}

/// Session management and orchestration.
pub mod sessions {
    #[doc(inline)]
    pub use polaris_internal::sessions::*;
}

/// Shell command execution with permission model.
pub mod shell {
    #[doc(inline)]
    pub use polaris_internal::shell::*;
}

/// HTTP server runtime with plugin integration.
pub mod app {
    #[doc(inline)]
    pub use polaris_internal::app::*;
}
