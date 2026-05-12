//! [`DashboardPlugin`] — registers [`DashboardRegistry`] and the manifest
//! endpoint.

use crate::registry::DashboardRegistry;
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use polaris_app::{AppPlugin, HttpRouter};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

/// Path served by the manifest handler.
const MANIFEST_PATH: &str = "/v1/dashboard/manifest";

/// Plugin that exposes the cross-plugin dashboard contribution registry.
///
/// Registers [`DashboardRegistry`] as a build-time API, mounts
/// `GET /v1/dashboard/manifest` against the registered [`HttpRouter`], and
/// freezes the registry in `ready()` so the manifest endpoint serves a
/// stable snapshot. The plugin is **opt-in** — it is not part of
/// `DefaultPlugins`.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only contributes APIs and HTTP routes. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`DashboardRegistry`] | Chained-builder surface (`add_nav_item`, `add_section`, `add_panel`, plus matching `remove_*`) for plugins to contribute dashboard descriptors. Also exposes a `tokio::sync::broadcast` channel of [`RegistryEvent`](crate::RegistryEvent)s. |
///
/// # Dependencies
///
/// - [`AppPlugin`] — required so the manifest endpoint can be registered
///   on its [`HttpRouter`].
///
/// # Example
///
/// ```no_run
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem};
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// struct ToolsContribution;
///
/// impl Plugin for ToolsContribution {
///     const ID: &'static str = "myapp::tools_dashboard";
///     const VERSION: Version = Version::new(0, 1, 0);
///
///     fn build(&self, server: &mut Server) {
///         server
///             .api::<DashboardRegistry>()
///             .expect("DashboardPlugin must be added first")
///             .add_nav_item(NavItem::new("tools", "Tools"));
///     }
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<DashboardPlugin>()]
///     }
/// }
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(DashboardPlugin)
///     .add_plugins(ToolsContribution);
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct DashboardPlugin;

impl Plugin for DashboardPlugin {
    const ID: &'static str = "polaris::dashboard";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_api(DashboardRegistry::new());

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added before DashboardPlugin")
            .add_routes_with(|server| {
                let registry = server
                    .api::<DashboardRegistry>()
                    .expect("DashboardRegistry must exist (registered in build)")
                    .clone();
                Router::new()
                    .route(MANIFEST_PATH, get(manifest_handler))
                    .with_state(registry)
            });
    }

    async fn ready(&self, server: &mut Server) {
        let registry = server
            .api::<DashboardRegistry>()
            .expect("DashboardRegistry must exist (registered in build)");
        registry.freeze();
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

/// `GET /v1/dashboard/manifest` — returns the frozen [`Manifest`] as JSON.
///
/// Serves the pre-serialized JSON bytes cached during
/// [`DashboardRegistry::freeze`], avoiding a per-request clone of the full
/// `Manifest`. Falls back to serializing the live snapshot during the brief
/// window before `freeze()` runs.
async fn manifest_handler(State(registry): State<DashboardRegistry>) -> Response {
    let body = registry.manifest_bytes();
    (
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body,
    )
        .into_response()
}
