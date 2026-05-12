//! Dashboard contributions for the sessions HTTP surface.

use crate::SessionsPlugin;
use crate::http::HttpPlugin;
use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem, Panel, Section, Transport};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

/// Dashboard contribution plugin for the canonical sessions panels.
///
/// This plugin contributes the sessions navigation entry plus three panels to
/// [`DashboardRegistry`]: the active sessions list, the agent graph detail
/// view, and the turn-stream log view.
///
/// Enabling the crate's `dashboard` feature also enables its `http` feature.
/// This is intentional: the dashboard contribution is only valid when the
/// underlying sessions HTTP endpoints are mounted.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin contributes dashboard descriptors only. |
///
/// # APIs Provided
///
/// None.
///
/// # Dependencies
///
/// - [`DashboardPlugin`]
/// - [`SessionsPlugin`]
/// - [`HttpPlugin`] — enforces the runtime invariant that the referenced
///   sessions endpoints are mounted.
///
/// # Example
///
/// ```no_run
/// # use std::sync::Arc;
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_core_plugins::PersistencePlugin;
/// use polaris_dashboard::DashboardPlugin;
/// use polaris_sessions::{
///     SessionsDashboardPlugin, SessionsPlugin,
///     http::HttpPlugin,
///     store::memory::InMemoryStore,
/// };
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(DashboardPlugin)
///     .add_plugins(HttpPlugin::new())
///     .add_plugins(SessionsDashboardPlugin);
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct SessionsDashboardPlugin;

impl Plugin for SessionsDashboardPlugin {
    const ID: &'static str = "polaris::sessions::dashboard";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before SessionsDashboardPlugin")
            .add_nav_item(NavItem::new("sessions", "Sessions"))
            .add_section(Section::new("sessions-overview", "sessions", "Overview"))
            .add_section(Section::new("sessions-detail", "sessions", "Detail"))
            .add_panel(
                Panel::new(
                    "sessions-list",
                    "Active sessions",
                    "list",
                    "/v1/sessions",
                    Transport::Rest,
                )
                .with_section("sessions-overview"),
            )
            .add_panel(
                Panel::new(
                    "sessions-graph",
                    "Agent graph",
                    "polaris-graph",
                    "/v1/sessions/{id}",
                    Transport::Rest,
                )
                .with_section("sessions-detail")
                .with_metadata(serde_json::json!({ "requires": ["session_id"] })),
            )
            .add_panel(
                Panel::new(
                    "sessions-turn-stream",
                    "Turn stream",
                    "log",
                    "/v1/sessions/{id}/turns/stream",
                    Transport::Sse,
                )
                .with_section("sessions-detail")
                .with_metadata(serde_json::json!({
                    "method": "POST",
                    "requires": ["session_id"],
                })),
            );
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<DashboardPlugin>(),
            PluginId::of::<SessionsPlugin>(),
            PluginId::of::<HttpPlugin>(),
        ]
    }
}
