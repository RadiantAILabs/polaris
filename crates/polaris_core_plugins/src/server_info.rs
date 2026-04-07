//! Server information plugin and resources.
//!
//! Provides [`ServerInfoPlugin`] which registers server metadata as a global resource.
//!
//! # Example
//!
//! ```
//! use polaris_system::server::Server;
//! use polaris_system::param::Res;
//! use polaris_system::system;
//! use polaris_core_plugins::{ServerInfoPlugin, ServerInfo};
//!
//! // Define a system that uses ServerInfo
//! #[system]
//! async fn log_server_info(info: Res<ServerInfo>) {
//!     println!("Polaris v{}", info.version);
//!     if info.debug {
//!         println!("Running in debug mode - extra logging enabled");
//!     }
//! }
//!
//! // Set up the server with the plugin
//! let mut server = Server::new();
//! server.add_plugins(ServerInfoPlugin);
//! # tokio_test::block_on(async {
//! server.finish().await;
//! # });
//!
//! // Access ServerInfo from a context
//! let ctx = server.create_context();
//! let info = ctx.get_resource::<ServerInfo>().unwrap();
//! assert!(!info.version.is_empty());
//! ```

use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;

/// Server runtime information.
///
/// Global resource providing metadata about the server runtime.
/// This is read-only and accessible by all agents via `Res<ServerInfo>`.
///
/// # Fields
///
/// - `version` - The framework version from `Cargo.toml`
/// - `debug` - Whether the server was compiled in debug mode
///
/// # Example
///
/// ```
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_core_plugins::ServerInfo;
///
/// #[system]
/// async fn check_environment(info: Res<ServerInfo>) {
///     println!("Polaris v{}", info.version);
///
///     if info.debug {
///         // Enable verbose logging in debug builds
///         println!("Debug mode: extra diagnostics enabled");
///     } else {
///         // Production optimizations
///         println!("Release mode: running optimized");
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Framework version string.
    pub version: &'static str,
    /// Whether running in debug mode.
    pub debug: bool,
}

impl GlobalResource for ServerInfo {}

impl Default for ServerInfo {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            debug: cfg!(debug_assertions),
        }
    }
}

/// Plugin that provides server metadata.
///
/// Registers [`ServerInfo`] as a global resource, providing server metadata
/// to all systems. This is a foundational plugin that most other plugins depend on.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ServerInfo`] | Global | Server metadata and runtime information |
///
/// # Dependencies
///
/// None. This is a foundational plugin with no dependencies.
///
/// # Example
///
/// ```
/// use polaris_system::server::Server;
/// use polaris_system::param::Res;
/// use polaris_system::system;
/// use polaris_core_plugins::{ServerInfoPlugin, ServerInfo};
///
/// // A system that adapts behavior based on build mode
/// #[system]
/// async fn adaptive_system(info: Res<ServerInfo>) -> String {
///     if info.debug {
///         format!("Debug build v{} - verbose mode", info.version)
///     } else {
///         format!("Release v{}", info.version)
///     }
/// }
///
/// // Register the plugin
/// let mut server = Server::new();
/// server.add_plugins(ServerInfoPlugin);
/// # tokio_test::block_on(async {
/// server.finish().await;
/// # });
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ServerInfoPlugin;

impl Plugin for ServerInfoPlugin {
    const ID: &'static str = "polaris::server_info";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_global(ServerInfo::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_info_default() {
        let info = ServerInfo::default();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn server_info_plugin_registers_resource() {
        let mut server = Server::new();
        server.add_plugins(ServerInfoPlugin);
        server.finish().await;

        let ctx = server.create_context();
        assert!(ctx.contains_resource::<ServerInfo>());
    }
}
