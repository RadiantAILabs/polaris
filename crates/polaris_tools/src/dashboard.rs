//! Dashboard snapshot endpoint for the tool registry.
//!
//! Activated by the `dashboard` feature. When the feature is on,
//! [`ToolsPlugin`](crate::ToolsPlugin) mounts `GET /v1/tools` and freezes a
//! pre-serialized snapshot of the registered tools during its `ready()`
//! phase.

use crate::ToolRegistry;
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use polaris_app::HttpRouter;
use polaris_models::llm::ToolDefinition;
use polaris_system::api::API;
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

const TOOLS_PATH: &str = "/v1/tools";
const EMPTY_TOOLS_RESPONSE: &[u8] = br#"{"items":[]}"#;

/// Build-time snapshot API backing the dashboard tools endpoint.
///
/// This is an internal coordination API, not a type a downstream consumer
/// reaches for directly. It carries a pre-serialized JSON snapshot of the
/// registered tools from [`ToolsPlugin`](crate::ToolsPlugin)'s `ready()` phase
/// to the `GET /v1/tools` route handler. Consumers who want the tool list
/// should call that HTTP endpoint, not resolve this API.
///
/// # Provided by
///
/// [`ToolsPlugin`](crate::ToolsPlugin) inserts it via `insert_api` during
/// `build()`, gated on the `dashboard` Cargo feature. Without that feature the
/// API is never registered and does not exist on the server.
///
/// # Surface
///
/// The type exposes **no public methods**. It is an internal coordination API
/// between [`ToolsPlugin`](crate::ToolsPlugin)'s `build()`/`ready()` phases and
/// the [`tools_snapshot_handler`] route handler, which holds it as axum
/// handler `State`.
///
/// # Lifecycle
///
/// - **`build()`** (feature `dashboard`) — [`ToolsPlugin`](crate::ToolsPlugin)
///   inserts the API and mounts the route with the snapshot as handler state.
/// - **`ready()`** — the snapshot is frozen exactly once from the globalized
///   [`ToolRegistry`]. Freezing is idempotent; the first freeze wins.
/// - **Runtime** — [`tools_snapshot_handler`] reads the frozen bytes
///   lock-free. Before the freeze the endpoint returns an empty
///   `{"items":[]}` envelope.
///
/// # Composition
///
/// **Single-replace** — only [`ToolsPlugin`](crate::ToolsPlugin) inserts this
/// API. It is never contributed to by other plugins.
///
/// # Example consumers
///
/// - [`tools_snapshot_handler`] — the `GET /v1/tools` route handler, which
///   receives the snapshot as axum `State` and serves its JSON bytes. No
///   plugin resolves this API via `server.api::<T>()`.
///
/// # Example
///
/// Adding [`ToolsPlugin`](crate::ToolsPlugin) is what makes this API exist
/// (with the `dashboard` feature on). It has no `server.api()` consumer in the
/// workspace — the snapshot is consumed internally by the `GET /v1/tools`
/// route handler:
///
/// ```no_run
/// use polaris_tools::ToolsPlugin;
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server.add_plugins(ToolsPlugin);
/// // With the `dashboard` feature enabled, ToolsPlugin inserts the
/// // ToolsSnapshot API internally and serves it at GET /v1/tools.
/// ```
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

/// Installs the dashboard snapshot API and HTTP route during
/// [`ToolsPlugin::build`](crate::ToolsPlugin).
pub(crate) fn install(server: &mut Server) {
    server.insert_api(ToolsSnapshot::new());

    server
        .api::<HttpRouter>()
        .expect("AppPlugin must be added before ToolsPlugin when `dashboard` is enabled")
        .add_routes_with(|server| {
            let snapshot = server
                .api::<ToolsSnapshot>()
                .expect("ToolsSnapshot must exist (registered in build)")
                .clone();
            Router::new()
                .route(TOOLS_PATH, get(tools_snapshot_handler))
                .with_state(snapshot)
        });
}

/// Freezes the snapshot from the now-globalized registry during
/// [`ToolsPlugin::ready`](crate::ToolsPlugin).
pub(crate) fn freeze(server: &Server) {
    let registry = server
        .get_global::<ToolRegistry>()
        .expect("ToolsPlugin must globalize ToolRegistry before dashboard freeze");
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
