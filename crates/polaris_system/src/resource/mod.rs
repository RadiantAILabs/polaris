//! Resource and output storage.
//!
//! This module provides containers for system data and marker traits for
//! resource scoping:
//!
//! - [`Resources`] - Type-safe storage for resources
//! - [`Outputs`] - Ephemeral system return values (cleared between runs)
//! - [`GlobalResource`] - Marker for read-only, server-lifetime resources
//! - [`LocalResource`] - Marker for mutable, per-context resources
//!
//! # Hierarchical Resource Model
//!
//! Resources can be scoped at different levels:
//!
//! | Scope | Marker Trait | Lifetime | Mutability |
//! |-------|--------------|----------|------------|
//! | Global | [`GlobalResource`] | Server | Read-only (`Res<T>`) |
//! | Local | [`LocalResource`] | Per-context | Mutable (`ResMut<T>`) |
//!
//! Global resources are registered via
//! [`Server::insert_global()`](crate::server::Server::insert_global) and shared
//! across all execution contexts. Local resources are registered via
//! [`Server::register_local()`](crate::server::Server::register_local) and
//! isolated per-context, enabling safe concurrent agent execution.
//!
//! # Example
//!
//! ```
//! use polaris_system::resource::{GlobalResource, LocalResource};
//! use polaris_system::param::{Res, ResMut};
//! use polaris_system::system;
//!
//! // Global resource - shared, read-only
//! struct Config { name: String }
//! impl GlobalResource for Config {}
//!
//! // Local resource - per-context, mutable
//! struct Memory { messages: Vec<String> }
//! impl LocalResource for Memory {}
//!
//! // Systems declare resources as parameters
//! #[system]
//! async fn my_system(
//!     config: Res<Config>,        // reads global
//!     mut memory: ResMut<Memory>, // mutates local
//! ) {
//!     memory.messages.push(config.name.clone());
//! }
//! ```

mod output;
#[expect(
    clippy::module_inception,
    reason = "resource.rs contains the core Resource trait and Resources container logic"
)]
mod resource;

pub use output::{Output, OutputError, OutputId, OutputRef, Outputs};
pub use resource::{
    ForkStrategy, GlobalResource, LocalResource, Resource, ResourceError, ResourceFactory,
    ResourceId, ResourceRef, ResourceRefMut, Resources,
};
