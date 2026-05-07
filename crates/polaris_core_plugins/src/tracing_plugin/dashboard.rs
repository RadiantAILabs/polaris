//! Dashboard contributions and HTTP endpoint for recent tracing records.

use super::{SpanBuffer, SpanBufferLayer, SpanRecord, TracingLayersApi, TracingPlugin};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use polaris_app::{AppPlugin, HttpRouter};
use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem, Panel, Transport};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};

const DEFAULT_LIMIT: usize = 200;
const TRACING_SPANS_PATH: &str = "/v1/tracing/spans";

#[derive(Debug, Default, Deserialize)]
struct SpansQuery {
    limit: Option<usize>,
}

/// Wire response for `GET /v1/tracing/spans`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpansResponse {
    /// Recent tracing records, oldest-to-newest within the selected window.
    pub items: Vec<SpanRecord>,
}

/// Dashboard plugin that captures recent tracing records and exposes them over
/// HTTP for the dashboard.
///
/// The plugin installs a [`SpanBufferLayer`] into [`TracingLayersApi`] during
/// `build()`, before [`TracingPlugin::ready`] assembles the subscriber. It
/// also mounts `GET /v1/tracing/spans` and contributes the tracing dashboard
/// panels.
///
/// The ring buffer defaults to 1024 records. `GET /v1/tracing/spans?limit=N`
/// returns the most recent `N` records, capped at the configured buffer
/// capacity. The layer emits records only for tracing events and span-close
/// notifications; it intentionally does not emit span-create or enter/exit
/// records. Expect roughly 5-10 µs of added work per event when enabled.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin contributes dashboard descriptors and an HTTP route only. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`SpanBuffer`] | Ring buffer consumed by the tracing dashboard endpoint. |
///
/// # Dependencies
///
/// - [`AppPlugin`]
/// - [`DashboardPlugin`]
/// - [`TracingPlugin`]
///
/// # Example
///
/// ```no_run
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_core_plugins::{
///     ServerInfoPlugin, TracingDashboardPlugin, TracingPlugin,
/// };
/// use polaris_dashboard::DashboardPlugin;
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(DashboardPlugin)
///     .add_plugins(TracingDashboardPlugin::new());
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TracingDashboardPlugin {
    capacity: usize,
}

impl Default for TracingDashboardPlugin {
    fn default() -> Self {
        Self {
            capacity: SpanBuffer::DEFAULT_CAPACITY,
        }
    }
}

impl TracingDashboardPlugin {
    /// Creates a new plugin with the default buffer capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the ring-buffer capacity.
    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }
}

impl Plugin for TracingDashboardPlugin {
    const ID: &'static str = "polaris::tracing::dashboard";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let buffer = SpanBuffer::with_capacity(self.capacity);
        server.insert_api(buffer.clone());

        server
            .get_resource_mut::<TracingLayersApi>()
            .expect("TracingPlugin must be added before TracingDashboardPlugin")
            .push(SpanBufferLayer::new(buffer.clone()));

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added before TracingDashboardPlugin")
            .add_routes_with(|server| {
                let buffer = server
                    .api::<SpanBuffer>()
                    .expect("SpanBuffer must exist (registered in build)")
                    .clone();
                Router::new()
                    .route(TRACING_SPANS_PATH, get(spans_handler))
                    .with_state(buffer)
            });

        let registry = server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before TracingDashboardPlugin");
        registry
            .add_nav_item(NavItem::new("tracing", "Tracing"))
            .add_panel(Panel::new(
                "tracing-spans",
                "Recent spans",
                "log",
                TRACING_SPANS_PATH,
                Transport::Rest,
            ));

        #[cfg(feature = "otel")]
        registry.add_panel(
            Panel::new(
                "tracing-otel-trace",
                "OTel trace tree",
                "otel-trace",
                TRACING_SPANS_PATH,
                Transport::Rest,
            )
            .with_metadata(serde_json::json!({ "format": "otel" })),
        );
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<AppPlugin>(),
            PluginId::of::<DashboardPlugin>(),
            PluginId::of::<TracingPlugin>(),
        ]
    }
}

/// `GET /v1/tracing/spans` — returns recent tracing records.
async fn spans_handler(
    State(buffer): State<SpanBuffer>,
    Query(query): Query<SpansQuery>,
) -> Json<SpansResponse> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(buffer.capacity());

    Json(SpansResponse {
        items: buffer.snapshot(limit),
    })
}
