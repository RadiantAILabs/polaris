//! Integration tests covering dashboard contributions from core plugins.

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    PersistencePlugin, ServerInfoPlugin, TracingDashboardPlugin, TracingPlugin,
};
use polaris_dashboard::{DashboardPlugin, Manifest, Transport};
use polaris_models::{ModelsDashboardPlugin, ModelsPlugin};
use polaris_sessions::{
    SessionsDashboardPlugin, SessionsPlugin, http::HttpPlugin, store::memory::InMemoryStore,
};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use polaris_tools::{ToolError, ToolRegistry, ToolsDashboardPlugin, ToolsPlugin, tool};
use std::collections::BTreeSet;
use std::sync::Arc;

#[tool]
/// A test fixture tool used by manifest-union integration tests.
async fn echo(
    /// The text to echo back.
    text: String,
) -> Result<String, ToolError> {
    Ok(text)
}

/// Registers a single fixture tool into the global `ToolRegistry`.
struct EchoToolFixture;

impl Plugin for EchoToolFixture {
    const ID: &'static str = "tests::echo_tool_fixture";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before EchoToolFixture")
            .register(echo());
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }
}

async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let port = listener
        .local_addr()
        .expect("ephemeral listener must expose local_addr")
        .port();
    (listener, port)
}

async fn wait_for_server(port: u16) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(10));
    loop {
        interval.tick().await;
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("server on port {port} did not become ready within 5 s");
        }
    }
}

async fn fetch_manifest(port: u16) -> Manifest {
    reqwest::get(format!("http://127.0.0.1:{port}/v1/dashboard/manifest"))
        .await
        .expect("manifest request must succeed")
        .json()
        .await
        .expect("manifest body must deserialize")
}

async fn fetch_items_envelope(port: u16, path: &str) -> serde_json::Value {
    let response = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
        .await
        .expect("endpoint request must succeed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "{path} must serve application/json",
    );

    let body: serde_json::Value = response.json().await.expect("body must be valid JSON");
    assert!(
        body.get("items")
            .and_then(serde_json::Value::as_array)
            .is_some(),
        "{path} must return an {{\"items\": [...]}} envelope",
    );
    body
}

async fn build_full_server() -> (Server, u16) {
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(DashboardPlugin)
        .add_plugins(ServerInfoPlugin)
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(ToolsPlugin)
        .add_plugins(EchoToolFixture)
        .add_plugins(ModelsPlugin)
        .add_plugins(TracingPlugin::new())
        .add_plugins(HttpPlugin::new())
        .add_plugins(SessionsDashboardPlugin)
        .add_plugins(ToolsDashboardPlugin)
        .add_plugins(ModelsDashboardPlugin)
        .add_plugins(TracingDashboardPlugin::new());
    server.finish().await;
    wait_for_server(port).await;
    (server, port)
}

async fn build_tools_only_server() -> (Server, u16) {
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(DashboardPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(EchoToolFixture)
        .add_plugins(ToolsDashboardPlugin);
    server.finish().await;
    wait_for_server(port).await;
    (server, port)
}

// `TracingPlugin` installs a process-global `tracing` subscriber, which makes
// it impossible to bootstrap a second server with `TracingPlugin` in the same
// test binary. The manifest-union and endpoint-round-trip assertions both need
// the full plugin set, so they share a single boot to keep the binary's
// tracing install happening exactly once.
#[tokio::test]
async fn full_server_unions_manifest_and_serves_endpoints() {
    let (mut server, port) = build_full_server().await;

    let manifest = fetch_manifest(port).await;
    let nav_ids: BTreeSet<_> = manifest
        .nav_items
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(
        nav_ids,
        BTreeSet::from(["models", "sessions", "tools", "tracing"]),
    );

    assert_eq!(
        manifest.sections.len(),
        1,
        "sessions should contribute one detail section"
    );
    let section = &manifest.sections[0];
    assert_eq!(section.id, "sessions-detail");
    assert_eq!(section.nav_item_id, "sessions");
    assert_eq!(section.title, "Detail");

    let panels: Vec<_> = manifest
        .panels
        .iter()
        .map(|panel| (panel.id.as_str(), panel.kind.as_str(), panel.transport))
        .collect();
    let required = [
        ("sessions-list", "list", Transport::Rest),
        ("sessions-graph", "polaris-graph", Transport::Rest),
        ("sessions-turn-stream", "log", Transport::Sse),
        ("tools-list", "list", Transport::Rest),
        ("models-providers", "list", Transport::Rest),
        ("tracing-spans", "log", Transport::Rest),
    ];
    for entry in &required {
        assert!(
            panels.contains(entry),
            "expected manifest to include panel {entry:?}, got {panels:?}",
        );
    }
    // The `otel` feature on `polaris_core_plugins` adds an extra
    // `tracing-otel-trace` panel; allow it but require nothing else.
    let allowed_extra = ("tracing-otel-trace", "otel-trace", Transport::Rest);
    for panel in &panels {
        assert!(
            required.contains(panel) || panel == &allowed_extra,
            "unexpected extra panel in manifest: {panel:?}",
        );
    }

    // Tools snapshot must surface the EchoToolFixture-registered tool.
    let tools = fetch_items_envelope(port, "/v1/tools").await;
    let tool_names: Vec<&str> = tools["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert!(
        tool_names.contains(&"echo"),
        "expected EchoToolFixture's tool in /v1/tools, got {tool_names:?}",
    );

    // No model providers are registered in this test bundle, so the
    // snapshot must be a structurally-valid empty list.
    let providers = fetch_items_envelope(port, "/v1/models/providers").await;
    let provider_items = providers["items"]
        .as_array()
        .expect("items must be an array");
    assert!(
        provider_items.is_empty(),
        "no ModelProvider plugins registered; expected empty providers list, got {provider_items:?}",
    );

    // Tracing spans is a ring buffer populated as `tracing::info!` calls
    // fire during startup; we cannot assert an exact count, but every
    // entry must have the documented record shape.
    let spans = fetch_items_envelope(port, "/v1/tracing/spans").await;
    for span in spans["items"].as_array().expect("items must be an array") {
        for field in ["name", "level", "target"] {
            assert!(
                span.get(field)
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "span entry must have string `{field}`, got {span}",
            );
        }
    }

    server.cleanup().await;
}

#[tokio::test]
async fn single_contributor_does_not_leak_others() {
    let (mut server, port) = build_tools_only_server().await;

    let manifest = fetch_manifest(port).await;
    let nav_ids: Vec<_> = manifest
        .nav_items
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(nav_ids, vec!["tools"]);
    assert!(
        manifest.sections.is_empty(),
        "tools should not contribute sections"
    );
    assert_eq!(manifest.panels.len(), 1);
    assert_eq!(manifest.panels[0].id, "tools-list");

    // Confirm `ToolsDashboardPlugin::ready` actually freezes the registered
    // tool. With an empty registry the snapshot endpoint is byte-identical to
    // the unfrozen fallback, so a fixture tool is required to distinguish the
    // two.
    let body = fetch_items_envelope(port, "/v1/tools").await;
    let names: Vec<&str> = body["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(names, vec!["echo"]);

    server.cleanup().await;
}

#[tokio::test]
async fn tools_dashboard_plugin_resolves_dependencies_when_added_first() {
    // Add `ToolsDashboardPlugin` before its declared dependencies. If
    // `dependencies()` is incomplete or ignored by the plugin sorter, this
    // call would panic on a missing API at build time.
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(ToolsDashboardPlugin)
        .add_plugins(EchoToolFixture)
        .add_plugins(ToolsPlugin)
        .add_plugins(DashboardPlugin)
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        );
    server.finish().await;
    wait_for_server(port).await;

    let body = fetch_items_envelope(port, "/v1/tools").await;
    let names: Vec<&str> = body["items"]
        .as_array()
        .expect("items must be an array")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect();
    assert_eq!(names, vec!["echo"]);

    server.cleanup().await;
}
