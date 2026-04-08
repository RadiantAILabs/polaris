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

/// Layer 1: ECS-inspired system framework.
#[doc(inline)]
pub use polaris_internal::polaris_system as system_crate;

/// Layer 2: Graph-based execution primitives.
#[doc(inline)]
pub use polaris_internal::polaris_graph as graph_crate;

/// Layer 2: Agent pattern definition.
#[doc(inline)]
pub use polaris_internal::polaris_agent as agent_crate;

/// Tool framework for LLM-callable functions.
#[doc(inline)]
pub use polaris_internal::polaris_tools as tools_crate;

/// Model provider implementations.
#[doc(inline)]
pub use polaris_internal::polaris_model_providers as model_providers_crate;

/// Model provider interface and registry.
#[doc(inline)]
pub use polaris_internal::polaris_models as models_crate;

/// Core infrastructure plugins (e.g., time, tracing).
#[doc(inline)]
pub use polaris_internal::polaris_core_plugins as core_plugins_crate;

/// Session management and orchestration.
#[doc(inline)]
pub use polaris_internal::polaris_sessions as sessions_crate;

/// Shell command execution.
#[doc(inline)]
pub use polaris_internal::polaris_shell as shell_crate;

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
