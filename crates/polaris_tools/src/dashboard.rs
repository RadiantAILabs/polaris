//! Dashboard contributions and snapshot endpoint for tools.

use crate::ToolRegistry;
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use polaris_app::{AppPlugin, HttpRouter};
use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem, Panel, Section, Transport};
use polaris_models::llm::ToolDefinition;
use polaris_system::api::API;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

const TOOLS_PATH: &str = "/v1/tools";
const EMPTY_TOOLS_RESPONSE: &[u8] = br#"{"items":[]}"#;

/// Build-time snapshot API for the dashboard tools endpoint.
///
/// [`ToolsDashboardPlugin`] registers this API during `build()`, then freezes
/// the tool list once in `ready()` after [`crate::ToolsPlugin`] has globalized
/// the registry. The HTTP handler serves the pre-serialized bytes lock-free.
#[derive(Debug, Clone, Default)]
pub struct ToolsSnapshot {
    frozen: Arc<OnceLock<Bytes>>,
}

impl API for ToolsSnapshot {}

impl ToolsSnapshot {
    fn new() -> Self {
        Self::default()
    }

    fn freeze(&self, definitions: Vec<ToolDefinitionWire>) {
        let json = serialize_tools_response(&ToolsResponse { items: definitions });
        let _ = self.frozen.set(json);
    }

    fn json_bytes(&self) -> Bytes {
        self.frozen
            .get()
            .map_or_else(|| Bytes::from_static(EMPTY_TOOLS_RESPONSE), Clone::clone)
    }
}

/// Wire response for `GET /v1/tools`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolsResponse {
    /// Tool definitions exposed through the snapshot endpoint.
    pub items: Vec<ToolDefinitionWire>,
}

/// Dashboard wire representation of a registered tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolDefinitionWire {
    /// Stable tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Effective permission level (`allow`, `confirm`, or `deny`).
    pub permission: String,
    /// JSON Schema for the tool's arguments. Type-erased intentionally —
    /// JSON Schema is dynamic by nature.
    pub parameters: serde_json::Value,
}

fn serialize_tools_response(response: &ToolsResponse) -> Bytes {
    let bytes = serde_json::to_vec(response)
        .expect("ToolsResponse serialization is infallible for our wire types");
    Bytes::from(bytes)
}

fn to_wire_definition(registry: &ToolRegistry, definition: ToolDefinition) -> ToolDefinitionWire {
    let permission = registry
        .permission(&definition.name)
        .expect("tool definition must correspond to a registered tool")
        .to_string();
    ToolDefinitionWire {
        name: definition.name,
        description: definition.description,
        permission,
        parameters: definition.parameters,
    }
}

/// Plugin that contributes the canonical tools dashboard view and mounts
/// `GET /v1/tools`.
///
/// Uses a small [`ToolsSnapshot`] API to pre-serialize the tools response
/// during `ready()`, so requests only clone shared [`Bytes`] from an
/// [`OnceLock`].
///
/// If you also enable `polaris_core_plugins/tools_tracing`, add
/// `TracingPlugin` before `ToolsDashboardPlugin` so the snapshot captures the
/// tracing-decorated registry.
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
/// | [`ToolsSnapshot`] | Frozen tools snapshot consumed by `GET /v1/tools`. |
///
/// # Dependencies
///
/// - [`AppPlugin`]
/// - [`DashboardPlugin`]
/// - [`crate::ToolsPlugin`]
///
/// # Example
///
/// ```no_run
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_dashboard::DashboardPlugin;
/// use polaris_system::server::Server;
/// use polaris_tools::{ToolsDashboardPlugin, ToolsPlugin};
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ToolsPlugin)
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(DashboardPlugin)
///     .add_plugins(ToolsDashboardPlugin);
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ToolsDashboardPlugin;

impl Plugin for ToolsDashboardPlugin {
    const ID: &'static str = "polaris::tools::dashboard";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_api(ToolsSnapshot::new());

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added before ToolsDashboardPlugin")
            .add_routes_with(|server| {
                let snapshot = server
                    .api::<ToolsSnapshot>()
                    .expect("ToolsSnapshot must exist (registered in build)")
                    .clone();
                Router::new()
                    .route(TOOLS_PATH, get(tools_snapshot_handler))
                    .with_state(snapshot)
            });

        server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before ToolsDashboardPlugin")
            .add_nav_item(NavItem::new("tools", "Tools"))
            .add_section(Section::new("tools-overview", "tools", "Overview"))
            .add_panel(
                Panel::new(
                    "tools-list",
                    "Available tools",
                    "list",
                    TOOLS_PATH,
                    Transport::Rest,
                )
                .with_section("tools-overview"),
            );
    }

    async fn ready(&self, server: &mut Server) {
        let registry = server
            .get_global::<ToolRegistry>()
            .expect("ToolsPlugin must globalize ToolRegistry before dashboard ready");
        let definitions = registry
            .definitions()
            .into_iter()
            .map(|definition| to_wire_definition(&registry, definition))
            .collect();

        server
            .api::<ToolsSnapshot>()
            .expect("ToolsSnapshot must exist from build phase")
            .freeze(definitions);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<AppPlugin>(),
            PluginId::of::<DashboardPlugin>(),
            PluginId::of::<crate::ToolsPlugin>(),
        ]
    }
}

/// `GET /v1/tools` — returns the frozen dashboard tool snapshot as JSON.
pub async fn tools_snapshot_handler(State(snapshot): State<ToolsSnapshot>) -> Response {
    (
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        snapshot.json_bytes(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(name: &str) -> ToolDefinitionWire {
        ToolDefinitionWire {
            name: name.into(),
            description: "desc".into(),
            permission: "allow".into(),
            parameters: serde_json::json!({}),
        }
    }

    #[test]
    fn json_bytes_returns_static_empty_envelope_before_freeze() {
        let snapshot = ToolsSnapshot::new();
        assert_eq!(&snapshot.json_bytes()[..], EMPTY_TOOLS_RESPONSE);
    }

    #[test]
    fn freeze_serves_pre_serialized_response() {
        let snapshot = ToolsSnapshot::new();
        snapshot.freeze(vec![wire("ls"), wire("rm")]);

        let bytes = snapshot.json_bytes();
        let response: ToolsResponse =
            serde_json::from_slice(&bytes).expect("frozen bytes must round-trip");
        assert_eq!(
            response
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ls", "rm"],
        );
    }

    #[test]
    fn json_bytes_serves_shared_buffer_after_freeze() {
        let snapshot = ToolsSnapshot::new();
        snapshot.freeze(vec![wire("ls")]);

        let first = snapshot.json_bytes();
        let second = snapshot.json_bytes();
        assert_eq!(first.as_ptr(), second.as_ptr());
    }

    #[test]
    fn freeze_is_idempotent() {
        let snapshot = ToolsSnapshot::new();
        snapshot.freeze(vec![wire("ls")]);
        snapshot.freeze(vec![wire("rm"), wire("cp")]);

        let response: ToolsResponse =
            serde_json::from_slice(&snapshot.json_bytes()).expect("frozen bytes must round-trip");
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].name, "ls");
    }
}
