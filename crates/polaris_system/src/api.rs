//! API trait for capability registration.
//!
//! An [`API`] is a build-time registry that plugins use to expose
//! capabilities to other plugins. APIs are accessed by plugins during
//! server setup via [`Server::api()`](crate::server::Server::api), unlike
//! [resources](crate::resource) which are accessed by systems at execution
//! time. See [`API`] for usage examples.

/// Marker trait for capability APIs.
///
/// APIs are build-time registries that plugins use to expose capabilities
/// to other plugins. They are accessed via
/// [`Server::api`](crate::server::Server::api) during the build and ready
/// phases, not by systems during execution.
///
/// Since `server.api::<T>()` returns `&T`, APIs that need mutable
/// registration typically use interior mutability (e.g. `RwLock`).
///
/// # Example
///
/// A provider plugin inserts the API during `build`; consumer plugins
/// access it during `ready`:
///
/// ```
/// use polaris_system::api::API;
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// struct MyAPI;
/// impl API for MyAPI {}
///
/// impl MyAPI { fn new() -> Self { MyAPI } fn register(&self, _: &str, _: i32) {} }
///
/// struct MyAPIPlugin;
///
/// impl Plugin for MyAPIPlugin {
///     const ID: &'static str = "my_api";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn build(&self, server: &mut Server) {
///         server.insert_api(MyAPI::new());
///     }
/// }
///
/// struct ConsumerPlugin;
///
/// impl Plugin for ConsumerPlugin {
///     const ID: &'static str = "consumer";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<MyAPIPlugin>()]
///     }
///
///     fn build(&self, _server: &mut Server) {}
///
///     async fn ready(&self, server: &mut Server) {
///         let api = server.api::<MyAPI>()
///             .expect("MyAPI required");
///         api.register("key", 42);
///     }
/// }
/// ```
pub trait API: Send + Sync + 'static {}
