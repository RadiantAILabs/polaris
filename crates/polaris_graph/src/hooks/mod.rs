//! Lifecycle hooks for graph execution.
//!
//! This module provides a hook system that allows plugins to extend the graph
//! executor with lifecycle callbacks. Hooks enable observability and resource
//! injection at various points during graph execution.
//!
//! # Design Principles
//!
//! - Hooks execute in registration order
//! - Primary use case: tracing, metrics, logging, debugging, resource injection
//! - Observer/Provider pattern separates concerns
//!
//! # Observer vs Provider
//!
//! - **Observers**: React to events (logging, metrics)
//! - **Providers**: Inject resources via return value
//!
//! When multiple providers write the same resource type, last-write-wins.
//!
//! # Architecture
//!
//! The hook system consists of three parts:
//!
//! - **Schedule markers** ([`schedule`]): Empty types that identify hook points
//! - **Events** ([`events`]): `GraphEvent` enum carrying context to hooks
//! - **API** ([`api`]): Registration and invocation mechanism
//!
//! # Example
//!
//! ```
//! # use polaris_graph::hooks::{HooksAPI, events::GraphEvent, schedule::{OnSystemStart, OnSystemComplete, OnSystemError}};
//! # fn example(hooks: &mut HooksAPI) -> Result<(), Box<dyn std::error::Error>> {
//! // Observer to log events
//! hooks.register_observer::<OnSystemStart, _>("logger", |event: &GraphEvent| {
//!     if let GraphEvent::SystemStart { node_name, .. } = event {
//!         println!("System {} starting", node_name);
//!     }
//! })?;
//!
//! // Multi-schedule observer
//! hooks.register_observer::<(OnSystemStart, OnSystemComplete, OnSystemError), _>(
//!     "tracker",
//!     |event: &GraphEvent| match event {
//!         GraphEvent::SystemStart { node_name, .. } => println!("Start: {}", node_name),
//!         GraphEvent::SystemComplete { duration, .. } => println!("Done: {:?}", duration),
//!         GraphEvent::SystemError { error, .. } => println!("Error: {}", error),
//!         _ => {}
//!     },
//! )?;
//!
//! // Provider that injects SystemInfo resource via return value
//! # use polaris_system::resource::LocalResource;
//! # struct SystemInfo { node_id: polaris_graph::node::NodeId, name: &'static str }
//! # impl SystemInfo { fn new(n: polaris_graph::node::NodeId, s: &'static str) -> Self { Self { node_id: n, name: s } } }
//! # impl LocalResource for SystemInfo {}
//! hooks.register_provider::<OnSystemStart, SystemInfo, _>("devtools", |event: &GraphEvent| {
//!     if let GraphEvent::SystemStart { node_id, node_name } = event {
//!         Some(SystemInfo::new(node_id.clone(), node_name))
//!     } else {
//!         None
//!     }
//! })?;
//!
//! // For direct access to the system context, register directly with register_boxed:
//! # use core::any::TypeId;
//! # use polaris_graph::hooks::api::BoxedHook;
//! # use polaris_system::plugin::ScheduleId;
//! # struct CustomResource;
//! let schedule = ScheduleId::of::<OnSystemStart>();
//! hooks.register_boxed(schedule, "custom", BoxedHook::new(
//!     move |_ctx, _event: &GraphEvent| {
//!         // custom logic here, with access to ctx for resource injection
//!     },
//!     vec![TypeId::of::<CustomResource>()], // declare provided resource type
//! ))?;
//! # Ok(())
//! # }
//! ```

pub mod api;
pub mod events;
pub mod schedule;

pub use api::{HookRegistrationError, HooksAPI};
pub use events::GraphEvent;
