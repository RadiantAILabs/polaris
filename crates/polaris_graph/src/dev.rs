//! Development tools for graph execution.
//!
//! The [`DevToolsPlugin`] injects [`SystemInfo`] before each system runs,
//! providing execution context for debugging and observability. It can
//! optionally emit `tracing::debug!` logs for all graph execution events.
//!
//! # Example
//!
//! ```
//! use polaris_graph::dev::SystemInfo;
//! use polaris_system::param::Res;
//! use polaris_system::system;
//!
//! #[system]
//! async fn my_system(info: Res<SystemInfo>) {
//!     eprintln!(
//!         "Executing system '{}' on node {:?}",
//!         info.system_name(),
//!         info.node_id()
//!     );
//! }
//! ```
//!
//! # Setup
//!
//! Add `DevToolsPlugin` to your server:
//!
//! ```
//! use polaris_graph::dev::DevToolsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! // Default: SystemInfo injection only
//! server.add_plugins(DevToolsPlugin::default());
//! ```
//!
//! Or with event tracing enabled:
//!
//! ```
//! use polaris_graph::dev::DevToolsPlugin;
//! use polaris_system::server::Server;
//!
//! let mut server = Server::new();
//! server.add_plugins(DevToolsPlugin::new().with_event_tracing());
//! ```
//!
//! # Validation
//!
//! `SystemInfo` is recognized as a hook-provided resource. Systems that declare
//! `Res<SystemInfo>` will not fail resource validation, as we leverage
//! `register_provider` api to track provided resource types.

use crate::hooks::HooksAPI;
use crate::hooks::events::GraphEvent;
use crate::hooks::schedule::{AllGraphSchedules, OnSystemStart};
use crate::node::NodeId;
use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;

/// Execution context injected by [`DevToolsPlugin`] before each system runs.
///
/// This resource provides information about the currently executing system,
/// useful for logging, debugging, and observability.
///
/// # Thread Safety
///
/// `SystemInfo` is a [`LocalResource`], meaning each execution context has
/// its own copy. It is updated by the [`DevToolsPlugin`] hook before each
/// system call.
#[derive(Debug, Clone)]
pub struct SystemInfo {
    /// The ID of the node currently being executed.
    node_id: NodeId,
    /// The name of the system being executed.
    system_name: &'static str,
}

impl LocalResource for SystemInfo {}

impl SystemInfo {
    /// Creates a new `SystemInfo` with the given execution context.
    pub fn new(node_id: NodeId, system_name: &'static str) -> Self {
        Self {
            node_id,
            system_name,
        }
    }

    /// Returns the ID of the node currently being executed.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.node_id.clone()
    }

    /// Returns the name of the system currently being executed.
    #[must_use]
    pub fn system_name(&self) -> &'static str {
        self.system_name
    }
}

/// Plugin that injects [`SystemInfo`] before each system execution.
///
/// This plugin registers a hook on [`OnSystemStart`] that injects a
/// [`SystemInfo`] resource into the context, making execution metadata
/// available to systems via `Res<SystemInfo>`.
///
/// When event tracing is enabled via [`with_event_tracing`](Self::with_event_tracing),
/// it also registers an observer on all graph execution schedules that emits
/// `tracing::debug!` logs for each event.
///
/// # Example
///
/// ```
/// use polaris_graph::dev::DevToolsPlugin;
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server.add_plugins(DevToolsPlugin::new().with_event_tracing());
/// ```
pub struct DevToolsPlugin {
    /// Whether to register a debug-level tracing observer for all graph events.
    trace_events: bool,
}

impl DevToolsPlugin {
    /// Creates a new `DevToolsPlugin` with event tracing disabled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            trace_events: false,
        }
    }

    /// Enables debug-level tracing for all graph execution events.
    ///
    /// When enabled, every graph lifecycle event is logged via `tracing::debug!`.
    #[must_use]
    pub fn with_event_tracing(mut self) -> Self {
        self.trace_events = true;
        self
    }
}

impl Default for DevToolsPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for DevToolsPlugin {
    const ID: &'static str = "polaris::dev_tools";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        if !server.contains_api::<HooksAPI>() {
            server.insert_api(HooksAPI::new());
        }

        let hooks = server
            .api::<HooksAPI>()
            .expect("HooksAPI should be present after initialization");

        // Register provider hook to inject SystemInfo before each system.
        hooks
            .register_provider::<OnSystemStart, _, _>(
                "devtools_system_info",
                |event: &GraphEvent| {
                    if let GraphEvent::SystemStart {
                        node_id,
                        node_name: system_name,
                    } = event
                    {
                        Some(SystemInfo::new(node_id.clone(), system_name))
                    } else {
                        None
                    }
                },
            )
            .expect("DevToolsPlugin hook registration should not fail");

        // Optionally register a debug-level tracing observer for all events.
        if self.trace_events {
            hooks
                .register_observer::<AllGraphSchedules, _>(
                    "devtools_event_trace",
                    |event: &GraphEvent| {
                        tracing::debug!("{event}");
                    },
                )
                .expect("DevToolsPlugin event tracing registration should not fail");
        }
    }
}
