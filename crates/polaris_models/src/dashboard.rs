//! Dashboard snapshot endpoint for the model provider registry.
//!
//! Activated by the `dashboard` feature. When the feature is on,
//! [`ModelsPlugin`](crate::ModelsPlugin) mounts `GET /v1/models/providers`
//! and freezes a pre-serialized snapshot of registered LLM providers during
//! its `ready()` phase.

use crate::ModelRegistry;
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use polaris_app::HttpRouter;
use polaris_system::api::API;
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

const PROVIDERS_PATH: &str = "/v1/models/providers";
const EMPTY_PROVIDERS_RESPONSE: &[u8] = br#"{"items":[]}"#;

/// Build-time snapshot API backing the dashboard model-provider endpoint.
///
/// This is an internal coordination API, not a type a downstream consumer
/// reaches for directly. It carries a pre-serialized JSON snapshot of the
/// registered LLM providers from [`ModelsPlugin`](crate::ModelsPlugin)'s
/// `ready()` phase to the `GET /v1/models/providers` route handler. Consumers
/// who want provider data should call that HTTP endpoint, not resolve this API.
///
/// # Provided by
///
/// [`ModelsPlugin`](crate::ModelsPlugin) inserts it via `insert_api` during
/// `build()`, gated on the `dashboard` Cargo feature. Without that feature the
/// API is never registered and does not exist on the server.
///
/// # Surface
///
/// The type exposes **no public methods**. It is an internal coordination API
/// between [`ModelsPlugin`](crate::ModelsPlugin)'s `build()`/`ready()` phases
/// and the [`models_snapshot_handler`] route handler, which holds it as axum
/// handler `State`.
///
/// # Lifecycle
///
/// - **`build()`** (feature `dashboard`) — [`ModelsPlugin`](crate::ModelsPlugin)
///   inserts the API and mounts the route with the snapshot as handler state.
/// - **`ready()`** — the snapshot is frozen exactly once from the globalized
///   [`ModelRegistry`]. Freezing is idempotent; the first freeze wins.
/// - **Runtime** — [`models_snapshot_handler`] reads the frozen bytes
///   lock-free. Before the freeze the endpoint returns an empty
///   `{"items":[]}` envelope.
///
/// # Composition
///
/// **Single-replace** — only [`ModelsPlugin`](crate::ModelsPlugin) inserts
/// this API. It is never contributed to by other plugins.
///
/// # Example consumers
///
/// - [`models_snapshot_handler`] — the `GET /v1/models/providers` route
///   handler, which receives the snapshot as axum `State` and serves its
///   JSON bytes. No plugin resolves this API via `server.api::<T>()`.
///
/// # Example
///
/// Adding [`ModelsPlugin`](crate::ModelsPlugin) is what makes this API exist
/// (with the `dashboard` feature on). It has no `server.api()` consumer in the
/// workspace — the snapshot is consumed internally by the
/// `GET /v1/models/providers` route handler:
///
/// ```no_run
/// use polaris_models::ModelsPlugin;
/// use polaris_system::server::Server;
///
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// // With the `dashboard` feature enabled, ModelsPlugin inserts the
/// // ModelsSnapshot API internally and serves it at GET /v1/models/providers.
/// ```
#[derive(Debug, Clone, Default)]
pub struct ModelsSnapshot {
    frozen: Arc<OnceLock<Bytes>>,
}

impl API for ModelsSnapshot {}

impl ModelsSnapshot {
    fn new() -> Self {
        Self::default()
    }

    fn freeze(&self, items: Vec<ModelProviderWire>) {
        let json = serialize_models_response(&ModelsProvidersResponse { items });
        let _ = self.frozen.set(json);
    }

    fn json_bytes(&self) -> Bytes {
        self.frozen.get().map_or_else(
            || Bytes::from_static(EMPTY_PROVIDERS_RESPONSE),
            Clone::clone,
        )
    }
}

/// Wire response for `GET /v1/models/providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelsProvidersResponse {
    /// Registered LLM providers.
    pub items: Vec<ModelProviderWire>,
}

/// Wire representation of a single registered LLM provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelProviderWire {
    /// Provider identifier such as `openai` or `anthropic`.
    pub name: String,
}

fn serialize_models_response(response: &ModelsProvidersResponse) -> Bytes {
    let bytes = serde_json::to_vec(response)
        .expect("ModelsProvidersResponse serialization is infallible for our wire types");
    Bytes::from(bytes)
}

/// Installs the dashboard snapshot API and HTTP route during
/// [`ModelsPlugin::build`](crate::ModelsPlugin).
pub(crate) fn install(server: &mut Server) {
    server.insert_api(ModelsSnapshot::new());

    server
        .api::<HttpRouter>()
        .expect("AppPlugin must be added before ModelsPlugin when `dashboard` is enabled")
        .add_routes_with(|server| {
            let snapshot = server
                .api::<ModelsSnapshot>()
                .expect("ModelsSnapshot must exist (registered in build)")
                .clone();
            Router::new()
                .route(PROVIDERS_PATH, get(models_snapshot_handler))
                .with_state(snapshot)
        });
}

/// Freezes the snapshot from the now-globalized registry during
/// [`ModelsPlugin::ready`](crate::ModelsPlugin).
pub(crate) fn freeze(server: &Server) {
    let registry = server
        .get_global::<ModelRegistry>()
        .expect("ModelsPlugin must globalize ModelRegistry before dashboard freeze");
    let mut names = registry.llm_provider_names();
    names.sort();
    let items = names
        .into_iter()
        .map(|name| ModelProviderWire { name })
        .collect();

    server
        .api::<ModelsSnapshot>()
        .expect("ModelsSnapshot must exist from build phase")
        .freeze(items);
}

/// `GET /v1/models/providers` — returns the frozen provider snapshot as JSON.
pub async fn models_snapshot_handler(State(snapshot): State<ModelsSnapshot>) -> Response {
    (
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        snapshot.json_bytes(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(name: &str) -> ModelProviderWire {
        ModelProviderWire { name: name.into() }
    }

    #[test]
    fn json_bytes_returns_static_empty_envelope_before_freeze() {
        let snapshot = ModelsSnapshot::new();
        assert_eq!(&snapshot.json_bytes()[..], EMPTY_PROVIDERS_RESPONSE);
    }

    #[test]
    fn freeze_serves_pre_serialized_response() {
        let snapshot = ModelsSnapshot::new();
        snapshot.freeze(vec![wire("anthropic"), wire("openai")]);

        let bytes = snapshot.json_bytes();
        let response: ModelsProvidersResponse =
            serde_json::from_slice(&bytes).expect("frozen bytes must round-trip");
        assert_eq!(
            response
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["anthropic", "openai"],
        );
    }

    #[test]
    fn json_bytes_serves_shared_buffer_after_freeze() {
        let snapshot = ModelsSnapshot::new();
        snapshot.freeze(vec![wire("anthropic")]);

        let first = snapshot.json_bytes();
        let second = snapshot.json_bytes();
        assert_eq!(first.as_ptr(), second.as_ptr());
    }

    #[test]
    fn freeze_is_idempotent() {
        let snapshot = ModelsSnapshot::new();
        snapshot.freeze(vec![wire("anthropic")]);
        snapshot.freeze(vec![wire("openai"), wire("bedrock")]);

        let response: ModelsProvidersResponse =
            serde_json::from_slice(&snapshot.json_bytes()).expect("frozen bytes must round-trip");
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].name, "anthropic");
    }
}
