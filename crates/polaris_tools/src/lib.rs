//! Tool framework for Polaris agents.
//!
//! This crate provides the infrastructure for defining, registering, and
//! executing tools that LLM agents can call. Tools are async functions
//! with automatic JSON schema generation and parameter injection.
//!
//! # Quick Start
//!
//! ```
//! use polaris_tools::{tool, ToolsPlugin, ToolRegistry, ToolError};
//! use polaris_system::plugin::{Plugin, PluginId, Version};
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(ToolsPlugin);
//! server.add_plugins(SearchPlugin);
//!
//! // Define a tool with the #[tool] macro
//! #[tool]
//! /// Search for documents matching a query.
//! async fn search(
//!     /// The search query.
//!     query: String,
//!     /// Max results to return.
//!     #[default(10)]
//!     limit: usize,
//! ) -> Result<String, ToolError> {
//!     Ok(format!("Found results for: {query}"))
//! }
//!
//! // Register in a plugin
//! struct SearchPlugin;
//!
//! impl Plugin for SearchPlugin {
//!     const ID: &'static str = "search";
//!     const VERSION: Version = Version::new(0, 0, 1);
//!
//!     fn dependencies(&self) -> Vec<PluginId> {
//!         vec![PluginId::of::<ToolsPlugin>()]
//!     }
//!
//!     fn build(&self, server: &mut Server) {
//!         let mut registry = server
//!             .get_resource_mut::<ToolRegistry>()
//!             .expect("ToolsPlugin must be added before SearchPlugin");
//!         registry.register(search());
//!     }
//! }
//! ```
//!
//! # Architecture
//!
//! - [`Tool`] — trait for executable tools with JSON schema
//! - [`Toolset`] — trait for grouped tools (via `#[toolset]`)
//! - [`ToolRegistry`] — stores and dispatches tools
//! - [`ToolsPlugin`] — manages registry lifecycle
//! - [`FunctionParam`] / [`InputParam`] — parameter extraction
//! - [`FunctionMetadata`] / [`ParameterInfo`] — schema building

// Self-reference so `#[tool]`/`#[toolset]` macro-generated code can use `polaris_tools::` paths
// within this crate.
extern crate self as polaris_tools;

pub mod builder;
pub mod error;
pub mod param;
pub mod permission;
pub mod registry;
pub mod schema;
pub mod tool;
pub mod toolset;

// Re-export core types at crate root.
pub use builder::{LlmReasonExt, LlmRequestBuilderExt, ReasonError};
pub use error::ToolError;
pub use param::{FunctionCall, FunctionParam, InputParam};
pub use permission::ToolPermission;
pub use registry::{ToolRegistry, ToolsPlugin};
pub use schema::{FunctionMetadata, ParameterInfo};
pub use tool::Tool;
pub use toolset::Toolset;

// Re-export proc macros.
pub use tool_macros::{tool, toolset};
