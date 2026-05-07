//! Dashboard contributions and snapshot endpoint for model providers.

use crate::ModelRegistry;
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use polaris_app::{AppPlugin, HttpRouter};
use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem, Panel, Transport};
use polaris_system::api::API;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

const PROVIDERS_PATH: &str = "/v1/models/providers";
const EMPTY_PROVIDERS_RESPONSE: &[u8] = br#"{"items":[]}"#;

/// Build-time snapshot API for the dashboard model-provider endpoint.
///
/// [`ModelsDashboardPlugin`] registers this API during `build()`, then
/// populates it in `ready()` after [`crate::ModelsPlugin`] has globalized the
/// registry.
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

/// Plugin that contributes the canonical model-provider dashboard view and
/// mounts `GET /v1/models/providers`.
///
/// Uses [`ModelsSnapshot`] to pre-serialize the provider list during `ready()`
/// and serves the cached bytes directly from the handler.
///
/// If you also enable `polaris_core_plugins/models_tracing`, add
/// `TracingPlugin` before `ModelsDashboardPlugin` so the snapshot captures the
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
/// | [`ModelsSnapshot`] | Frozen provider snapshot consumed by `GET /v1/models/providers`. |
///
/// # Dependencies
///
/// - [`AppPlugin`]
/// - [`DashboardPlugin`]
/// - [`crate::ModelsPlugin`]
///
/// # Example
///
/// ```no_run
/// use polaris_app::{AppConfig, AppPlugin};
/// use polaris_dashboard::DashboardPlugin;
/// use polaris_models::{ModelsDashboardPlugin, ModelsPlugin};
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ModelsPlugin)
///     .add_plugins(AppPlugin::new(AppConfig::new()))
///     .add_plugins(DashboardPlugin)
///     .add_plugins(ModelsDashboardPlugin);
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ModelsDashboardPlugin;

impl Plugin for ModelsDashboardPlugin {
    const ID: &'static str = "polaris::models::dashboard";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_api(ModelsSnapshot::new());

        server
            .api::<HttpRouter>()
            .expect("AppPlugin must be added before ModelsDashboardPlugin")
            .add_routes_with(|server| {
                let snapshot = server
                    .api::<ModelsSnapshot>()
                    .expect("ModelsSnapshot must exist (registered in build)")
                    .clone();
                Router::new()
                    .route(PROVIDERS_PATH, get(models_snapshot_handler))
                    .with_state(snapshot)
            });

        server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before ModelsDashboardPlugin")
            .add_nav_item(NavItem::new("models", "Models"))
            .add_panel(Panel::new(
                "models-providers",
                "LLM providers",
                "list",
                PROVIDERS_PATH,
                Transport::Rest,
            ));
    }

    async fn ready(&self, server: &mut Server) {
        let registry = server
            .get_global::<ModelRegistry>()
            .expect("ModelsPlugin must globalize ModelRegistry before dashboard ready");
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

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<AppPlugin>(),
            PluginId::of::<DashboardPlugin>(),
            PluginId::of::<crate::ModelsPlugin>(),
        ]
    }
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
