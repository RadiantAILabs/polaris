//! Core infrastructure plugins for Polaris.
//!
//! This crate provides foundational plugins that most Polaris applications need:
//!
//! - [`ServerInfoPlugin`] - Server metadata and runtime information
//! - [`TimePlugin`] - Time utilities with mockable clock for testing
//! - [`TracingPlugin`] - Logging and observability via the `tracing` crate
//! - [`IOPlugin`] - I/O abstractions for agent communication (opt-in)
//! - [`persistence::PersistencePlugin`] - Persistence registry for storable resources
//! - [`DefaultPlugins`] - Convenient bundle of all infrastructure plugins
//!
//! # Feature Flags
//!
//! - `test-utils` - Enables [`MockClock`] and [`MockIOProvider`] for testing
//!
//! # Example
//!
//! ```
//! use polaris_system::server::Server;
//! use polaris_system::plugin::PluginGroup;
//! use polaris_core_plugins::DefaultPlugins;
//!
//! Server::new()
//!     .add_plugins(DefaultPlugins::new().build())
//!     .run();
//! ```
//!
//! # Individual Plugin Usage
//!
//! For fine-grained control, add plugins individually:
//!
//! ```
//! use polaris_system::server::Server;
//! use polaris_core_plugins::{ServerInfoPlugin, TimePlugin, TracingPlugin};
//! use tracing::Level;
//!
//! Server::new()
//!     .add_plugins(ServerInfoPlugin)
//!     .add_plugins(TimePlugin::default())
//!     .add_plugins(TracingPlugin::default().with_level(Level::DEBUG))
//!     .run();
//! ```
//!
//! # Architecture
//!
//! This crate is part of Layer 1 infrastructure:
//!
//! - **Layer 1** (`polaris_system`, `polaris_core`): Core primitives and infrastructure
//! - **Layer 2** (`polaris_graph`, `polaris_agent`): Graph execution and agent patterns
//! - **Layer 3** (plugins): Concrete agent implementations

// Self-reference ensuring `#[derive(Storable)]` macro-generated code can use `polaris_core_plugins::` paths within this crate.
extern crate self as polaris_core_plugins;

mod io;
pub mod persistence;
mod server_info;
mod time;
mod tracing_plugin;

// Re-export plugins
pub use io::IOPlugin;
pub use server_info::ServerInfoPlugin;
pub use time::{Clock, ClockProvider, Stopwatch, TimePlugin};
pub use tracing_plugin::{TracingFormat, TracingPlugin};

// Re-export IO types
pub use io::{
    IOContent, IOError, IOMessage, IOProvider, IOSource, IOStream, InputBuffer, OutputBuffer,
    UserIO,
};

// Re-export test utilities
#[cfg(any(test, feature = "test-utils"))]
pub use io::MockIOProvider;
#[cfg(any(test, feature = "test-utils"))]
pub use time::MockClock;

// Re-export persistence types
pub use persistence::{
    PersistenceAPI, PersistenceError, PersistencePlugin, ResourceSerializer, Storable,
};

// Re-export resources
pub use server_info::ServerInfo;
pub use tracing_plugin::TracingConfig;

use polaris_system::plugin::{PluginGroup, PluginGroupBuilder};
use tracing::Level;

/// Default plugins for most Polaris applications.
///
/// Includes:
/// - [`ServerInfoPlugin`] - Server metadata
/// - [`TimePlugin`] - Time utilities
/// - [`TracingPlugin`] - Logging and observability
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::plugin::PluginGroup;
/// use polaris_core_plugins::DefaultPlugins;
///
/// Server::new()
///     .add_plugins(DefaultPlugins::new().build())
///     .run();
/// ```
///
/// # Customization
///
/// Configure tracing directly:
///
/// ```no_run
/// use polaris_system::server::Server;
/// use polaris_system::plugin::PluginGroup;
/// use polaris_core_plugins::{DefaultPlugins, TracingFormat};
/// use tracing::Level;
///
/// Server::new()
///     .add_plugins(
///         DefaultPlugins::new()
///             .with_log_level(Level::DEBUG)
///             .with_env_filter("polaris=debug,hyper=warn")
///             .with_tracing_format(TracingFormat::Json)
///             .build()
///     )
///     .run();
/// ```
///
/// Or disable a plugin entirely:
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::plugin::PluginGroup;
/// use polaris_core_plugins::{DefaultPlugins, TracingPlugin};
///
/// Server::new()
///     .add_plugins(
///         DefaultPlugins::new()
///             .build()
///             .disable::<TracingPlugin>()
///     )
///     .run();
/// ```
pub struct DefaultPlugins {
    /// Override for the tracing log level.
    log_level: Option<Level>,
    /// Override for the tracing output format.
    tracing_format: Option<TracingFormat>,
    /// Override for the tracing environment filter.
    env_filter: Option<String>,
    /// Override for span event logging.
    span_events: Option<bool>,
}

impl DefaultPlugins {
    /// Creates a new `DefaultPlugins` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log_level: None,
            tracing_format: None,
            env_filter: None,
            span_events: None,
        }
    }

    /// Sets the tracing log level.
    #[must_use]
    pub fn with_log_level(mut self, level: Level) -> Self {
        self.log_level = Some(level);
        self
    }

    /// Sets the tracing output format.
    #[must_use]
    pub fn with_tracing_format(mut self, format: TracingFormat) -> Self {
        self.tracing_format = Some(format);
        self
    }

    /// Sets a custom environment filter string for tracing.
    ///
    /// Format: `target=level,target=level,...`
    #[must_use]
    pub fn with_env_filter(mut self, filter: impl Into<String>) -> Self {
        self.env_filter = Some(filter.into());
        self
    }

    /// Enables or disables span enter/exit events in tracing output.
    #[must_use]
    pub fn with_span_events(mut self, enabled: bool) -> Self {
        self.span_events = Some(enabled);
        self
    }
}

impl Default for DefaultPlugins {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginGroup for DefaultPlugins {
    fn build(self) -> PluginGroupBuilder {
        let mut tracing = TracingPlugin::default();
        if let Some(level) = self.log_level {
            tracing = tracing.with_level(level);
        }
        if let Some(format) = self.tracing_format {
            tracing = tracing.with_format(format);
        }
        if let Some(filter) = self.env_filter {
            tracing = tracing.with_env_filter(filter);
        }
        if let Some(enabled) = self.span_events {
            tracing = tracing.with_span_events(enabled);
        }

        PluginGroupBuilder::new()
            .add(ServerInfoPlugin)
            .add(TimePlugin::default())
            .add(tracing)
    }
}

/// Minimal plugins for headless or testing scenarios.
///
/// Includes only:
/// - [`ServerInfoPlugin`] - Server metadata
/// - [`TimePlugin`] - Time utilities
///
/// Does not include tracing, making it suitable for unit tests
/// that don't need logging output.
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::plugin::PluginGroup;
/// use polaris_core_plugins::MinimalPlugins;
///
/// Server::new()
///     .add_plugins(MinimalPlugins.build())
///     .run();
/// ```
pub struct MinimalPlugins;

impl PluginGroup for MinimalPlugins {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::new()
            .add(ServerInfoPlugin)
            .add(TimePlugin::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_system::server::Server;

    #[test]
    fn default_plugins_builds() {
        let builder = DefaultPlugins::new().build();
        assert_eq!(builder.len(), 3);
    }

    #[test]
    fn default_plugins_with_options() {
        let builder = DefaultPlugins::new()
            .with_log_level(Level::DEBUG)
            .with_tracing_format(TracingFormat::Json)
            .with_env_filter("polaris=debug")
            .with_span_events(true)
            .build();
        assert_eq!(builder.len(), 3);
    }

    #[test]
    fn minimal_plugins_builds() {
        let builder = MinimalPlugins.build();
        assert_eq!(builder.len(), 2);
    }

    #[test]
    fn server_with_minimal_plugins() {
        let mut server = Server::new();
        server.add_plugins(MinimalPlugins.build());
        server.finish();

        // Verify ServerInfo resource is available
        let ctx = server.create_context();
        assert!(ctx.contains_resource::<ServerInfo>());
        assert!(ctx.contains_resource::<Clock>());
    }
}
