//! The foundational ECS-inspired framework for Polaris (Layer 1).
//!
//! `polaris_system` provides the core primitives for building AI agents:
//!
//! - [`api`] - API trait for capability registration
//! - [`param`] - System parameters and dependency injection
//! - [`plugin`] - Plugin trait for extensible functionality
//! - [`resource`] - Shared state management (Resources and Outputs)
//! - [`server`] - Server runtime for plugin orchestration
//! - [`mod@system`] - System trait and async function conversion
//! - [`macro@system`] - Attribute macro for defining systems
//!
//! # Architecture
//!
//! This crate is Layer 1 of the Polaris architecture:
//!
//! - **Layer 1** (`polaris_system`): ECS-inspired primitives (this crate)
//! - **Layer 2** (`polaris_graph`): Graph-based agent execution
//! - **Layer 3** (plugins): Concrete agent implementations, tools, LLMs
//!
//! # Example
//!
//! ```
//! use polaris_system::plugin::{Plugin, Version};
//! use polaris_system::server::Server;
//! use polaris_system::resource::GlobalResource;
//!
//! #[derive(Default)]
//! struct MyConfig { max_tokens: usize }
//! impl GlobalResource for MyConfig {}
//!
//! struct MyPlugin;
//!
//! impl Plugin for MyPlugin {
//!     const ID: &'static str = "my_plugin";
//!     const VERSION: Version = Version::new(0, 0, 1);
//!
//!     fn build(&self, server: &mut Server) {
//!         server.insert_global(MyConfig::default());
//!     }
//! }
//!
//! # tokio_test::block_on(async {
//! Server::new()
//!     .add_plugins(MyPlugin)
//!     .run()
//!     .await;
//! # });
//! ```

// Self-reference so `#[system]` macro-generated code can use `polaris_system::` paths
// within this crate.
extern crate self as polaris_system;

/// API trait for capability registration.
pub mod api;

/// System parameters and dependency injection.
pub mod param;

/// Plugin trait for extensible functionality.
pub mod plugin;

/// Resource and output container management.
pub mod resource;

/// Server runtime for plugin orchestration.
pub mod server;

/// System execution primitives.
pub mod system;

/// Re-export the `#[system]` attribute macro.
pub use polaris_system_macros::system;

/// Re-export all common types for easy access.
pub mod prelude {
    pub use crate::api::*;
    pub use crate::param::*;
    pub use crate::plugin::*;
    pub use crate::resource::*;
    pub use crate::server::*;
    pub use crate::system::*;
}
