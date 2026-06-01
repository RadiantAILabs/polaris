//! Provides the [`ModelRegistry`] global resource.

use crate::registry::ModelRegistry;
use polaris_system::plugin::{Plugin, PluginAccess, Version};
use polaris_system::server::Server;

/// Plugin that provides the [`ModelRegistry`] for provider-agnostic model access.
///
/// Use this whenever an agent needs to resolve an LLM by string identifier
/// such as `"anthropic/claude-sonnet-4-6"`. Provider plugins
/// (e.g., [`AnthropicPlugin`](polaris_model_providers::AnthropicPlugin))
/// register themselves with the registry; consumers later resolve typed
/// model handles via `Res<ModelRegistry>`.
///
/// # Lifecycle
///
/// The registry uses a two-phase initialization so providers can register
/// while runtime access remains immutable:
///
/// 1. **`build()` phase** — the registry is inserted as a mutable resource.
///    Provider plugins access it via [`Server::get_resource_mut`] and call
///    [`ModelRegistry::register_llm_provider`] to register themselves.
/// 2. **`ready()` phase** — the registry is moved from a mutable resource
///    to an immutable global, ensuring thread-safe read-only access during
///    agent execution.
///
/// With the `dashboard` feature on, `build()` additionally installs the
/// [`ModelsSnapshot`](crate::dashboard::ModelsSnapshot) API and the
/// `GET /v1/models/providers` route, and `ready()` freezes the provider
/// snapshot from the globalized registry. The feature also gates the
/// [`AppPlugin`](polaris_app::AppPlugin) dependency. The plugin registers
/// no tick schedules.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ModelRegistry`] | Global (post-`ready`) | Provider-agnostic registry mapping `provider/model` identifiers to typed LLM handles. Mutable during `build()`, frozen as a global resource in `ready()`. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`ModelsSnapshot`](crate::dashboard::ModelsSnapshot) *(feature `dashboard`)* | Frozen snapshot of registered providers consumed by `GET /v1/models/providers`. |
///
/// # Dependencies
///
/// - [`AppPlugin`](polaris_app::AppPlugin) — only when the `dashboard`
///   feature is enabled. The dashboard surface mounts an HTTP route via
///   [`HttpRouter`](polaris_app::HttpRouter), which `AppPlugin` provides.
///
/// # Routes Provided
///
/// Mounted only when the `dashboard` feature is enabled, against the
/// [`HttpRouter`](polaris_app::HttpRouter) owned by `AppPlugin`.
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `GET` | `/v1/models/providers` | Frozen snapshot of registered LLM provider identifiers. Takes no parameters — the handler reads only its axum `State`. |
///
/// # Extends
///
/// - [`HttpRouter`](polaris_app::HttpRouter) (from
///   [`AppPlugin`](polaris_app::AppPlugin)) *(feature `dashboard`)* —
///   mounts the `GET /v1/models/providers` snapshot route.
///
/// # Example
///
/// ```no_run
/// use polaris_models::ModelsPlugin;
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// // Then add one or more provider plugins, e.g. AnthropicPlugin::from_env(...)
/// ```
#[derive(Debug, Default, Copy, Clone)]
pub struct ModelsPlugin;

impl Plugin for ModelsPlugin {
    const ID: &'static str = "polaris::models";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_resource(ModelRegistry::new());

        #[cfg(feature = "dashboard")]
        crate::dashboard::install(server);
    }

    async fn ready(&self, server: &mut Server) {
        let model_registry = server.remove_resource::<ModelRegistry>().unwrap();
        server.insert_global(model_registry);

        #[cfg(feature = "dashboard")]
        crate::dashboard::freeze(server);
    }

    /// Declares that this plugin provides the [`ModelRegistry`] capability, so provider
    /// plugins can declare they extend it instead of naming `ModelsPlugin` directly.
    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<ModelRegistry>(ModelRegistry::CONTRACT_VERSION)
    }

    #[cfg(feature = "dashboard")]
    fn dependencies(&self) -> Vec<polaris_system::plugin::PluginId> {
        vec![polaris_system::plugin::PluginId::of::<polaris_app::AppPlugin>()]
    }
}
