//! # Polaris Internal Library
//!
//! Re-exports the core Polaris crates for convenience.

/// Layer 1: ECS-inspired system framework.
pub use polaris_system;

/// Layer 2: Graph-based execution primitives.
pub use polaris_graph;

/// Layer 2: Agent pattern definition.
pub use polaris_agent;

/// Tool framework for LLM-callable functions.
pub use polaris_tools;

/// Layer 3: Model providers and model-related utilities.
pub use polaris_model_providers;
pub use polaris_models;

/// Core infrastructure plugins (e.g., time, tracing).
pub use polaris_core_plugins;

/// Session management and orchestration.
pub use polaris_sessions;

/// Shell command execution.
pub use polaris_shell;

/// Re-export all common types for easy access.
pub mod prelude {
    pub use polaris_agent::Agent;
    pub use polaris_graph::prelude::*;
    pub use polaris_system::prelude::*;
    pub use polaris_tools::{
        LlmReasonExt, LlmRequestBuilderExt, ReasonError, Tool, ToolError, ToolRegistry,
        ToolsPlugin, Toolset,
    };
}

/// Re-export all system-related types for easy access.
pub mod system {
    pub use polaris_system::*;
}

/// Re-export all graph-related types for easy access.
pub mod graph {
    pub use polaris_graph::*;
}

/// Re-export all agent-related types for easy access.
pub mod agent {
    pub use polaris_agent::*;
}

/// Re-export all tool-related types for easy access.
pub mod tools {
    pub use polaris_tools::*;
}

/// Re-export all model-related types for easy access.
pub mod models {
    pub use polaris_model_providers::*;
    pub use polaris_models::*;
}

/// Re-export all core plugin types for easy access.
pub mod plugins {
    pub use polaris_core_plugins::*;
}

/// Re-export all session-related types for easy access.
pub mod sessions {
    pub use polaris_sessions::*;
}

/// Re-export all shell-related types for easy access.
pub mod shell {
    pub use polaris_shell::*;
}
