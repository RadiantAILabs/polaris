//! Integration tests for `GET /v1/dashboard/manifest`.
//!
//! Spins up a real `AppPlugin` + `DashboardPlugin` server on an ephemeral
//! port, contributes descriptors via a fixture plugin, and asserts the
//! served manifest matches the contributions and survives `freeze()`.

use polaris_app::{AppConfig, AppPlugin};
use polaris_dashboard::{DashboardPlugin, DashboardRegistry, NavItem, Panel, Section, Transport};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Fixture plugin that contributes a representative set of descriptors.
struct FixtureContribution;

impl Plugin for FixtureContribution {
    const ID: &'static str = "tests::fixture_contribution";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before fixture")
            .add_nav_item(NavItem::new("sessions", "Sessions"))
            .add_nav_item(NavItem::new("tools", "Tools"))
            .add_section(Section::new("active", "sessions", "Active"))
            .add_panel(
                Panel::new(
                    "sessions-list",
                    "All sessions",
                    "list",
                    "/v1/sessions",
                    Transport::Rest,
                )
                .with_section("active"),
            )
            .add_panel(Panel::new(
                "tools-list",
                "All tools",
                "list",
                "/v1/tools",
                Transport::Rest,
            ));
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<DashboardPlugin>()]
    }
}

/// Fixture plugin that suppresses one of `FixtureContribution`'s panels.
struct SuppressTools;

impl Plugin for SuppressTools {
    const ID: &'static str = "tests::suppress_tools";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server
            .api::<DashboardRegistry>()
            .expect("DashboardPlugin must be added before suppressor")
            .remove_nav_item("tools")
            .remove_panel("tools-list");
    }

    fn dependencies(&self) -> Vec<PluginId> {
        // Depend on the upstream plugin so we run after it (decision #15).
        vec![PluginId::of::<FixtureContribution>()]
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

async fn fetch_manifest(port: u16) -> serde_json::Value {
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/v1/dashboard/manifest"))
        .await
        .expect("manifest request must succeed");
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "manifest endpoint must serve application/json"
    );
    resp.json().await.expect("manifest body must be valid JSON")
}

async fn build_server<P: Plugin + 'static>(extra: P) -> (Server, u16) {
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(DashboardPlugin)
        .add_plugins(extra);
    server.finish().await;
    wait_for_server(port).await;
    (server, port)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn manifest_returns_contributions_after_ready() {
    let (mut server, port) = build_server(FixtureContribution).await;

    let body = fetch_manifest(port).await;
    let nav_ids: Vec<&str> = body["nav_items"]
        .as_array()
        .expect("nav_items must serialize as a JSON array")
        .iter()
        .map(|item| {
            item["id"]
                .as_str()
                .expect("each nav_item must have a string id")
        })
        .collect();
    assert_eq!(nav_ids, vec!["sessions", "tools"]);

    let panels = body["panels"]
        .as_array()
        .expect("panels must serialize as a JSON array");
    let panel_ids: Vec<&str> = panels
        .iter()
        .map(|panel| {
            panel["id"]
                .as_str()
                .expect("each panel must have a string id")
        })
        .collect();
    assert_eq!(panel_ids, vec!["sessions-list", "tools-list"]);

    // Inspect every panel — a regression in non-first panels would otherwise
    // slip past a positional assertion.
    assert_eq!(panels[0]["section_id"], "active");
    assert_eq!(panels[0]["transport"], "rest");
    assert_eq!(panels[1]["transport"], "rest");
    assert!(
        panels[1].get("section_id").is_none(),
        "tools-list has no section, so section_id should be omitted"
    );

    server.cleanup().await;
}

#[tokio::test]
async fn manifest_endpoint_serves_empty_collections_when_no_contributors() {
    // Realistic startup state: dashboard plugin is enabled but no contributors
    // are loaded yet. The endpoint must return the empty triple, not 404
    // or an error.
    struct EmptyContrib;
    impl Plugin for EmptyContrib {
        const ID: &'static str = "tests::empty_contrib";
        const VERSION: Version = Version::new(0, 0, 1);
        fn build(&self, _: &mut Server) {}
        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<DashboardPlugin>()]
        }
    }
    let (mut server, port) = build_server(EmptyContrib).await;

    let body = fetch_manifest(port).await;
    assert_eq!(
        body["nav_items"]
            .as_array()
            .expect("nav_items must be array")
            .len(),
        0
    );
    assert_eq!(
        body["sections"]
            .as_array()
            .expect("sections must be array")
            .len(),
        0
    );
    assert_eq!(
        body["panels"]
            .as_array()
            .expect("panels must be array")
            .len(),
        0
    );

    server.cleanup().await;
}

#[tokio::test]
async fn dependencies_reorder_dashboard_after_app_when_added_first() {
    // Add DashboardPlugin BEFORE AppPlugin to prove that
    // `DashboardPlugin::dependencies()` actually drives the topological sort.
    // Without dependency-driven reordering, `DashboardPlugin::build()` would
    // panic on the missing `HttpRouter` API.
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(DashboardPlugin)
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(FixtureContribution);
    server.finish().await;
    wait_for_server(port).await;

    let body = fetch_manifest(port).await;
    assert_eq!(
        body["nav_items"]
            .as_array()
            .expect("nav_items must be array")
            .len(),
        2,
        "DashboardPlugin must run after AppPlugin via dependency resolution"
    );

    server.cleanup().await;
}

#[tokio::test]
#[should_panic(expected = "requires")]
async fn dashboard_plugin_panics_when_app_plugin_missing() {
    // The framework's dependency validator must fail loudly when AppPlugin
    // isn't registered, so users get an actionable message instead of a
    // silent missing-route bug. The framework's panic format
    // ("Plugin '...' requires '...' which was not added") includes the word
    // `requires`; we pin on that token, not the full sentence, to stay
    // robust against framework wording tweaks.
    let mut server = Server::new();
    server.add_plugins(DashboardPlugin);
    server.finish().await;
}

#[tokio::test]
async fn downstream_plugin_can_suppress_upstream_contributions() {
    let (mut server, port) = build_server_chain().await;

    let body = fetch_manifest(port).await;
    let nav_ids: Vec<&str> = body["nav_items"]
        .as_array()
        .expect("nav_items must serialize as a JSON array")
        .iter()
        .map(|item| {
            item["id"]
                .as_str()
                .expect("each nav_item must have a string id")
        })
        .collect();
    assert_eq!(nav_ids, vec!["sessions"]);

    let panel_ids: Vec<&str> = body["panels"]
        .as_array()
        .expect("panels must serialize as a JSON array")
        .iter()
        .map(|panel| {
            panel["id"]
                .as_str()
                .expect("each panel must have a string id")
        })
        .collect();
    assert_eq!(panel_ids, vec!["sessions-list"]);

    server.cleanup().await;
}

async fn build_server_chain() -> (Server, u16) {
    let (listener, port) = bind_ephemeral().await;
    let mut server = Server::new();
    server
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(DashboardPlugin)
        .add_plugins(FixtureContribution)
        .add_plugins(SuppressTools);
    server.finish().await;
    wait_for_server(port).await;
    (server, port)
}

#[tokio::test]
async fn registry_is_frozen_after_finish() {
    let (mut server, _port) = build_server(FixtureContribution).await;

    let registry = server
        .api::<DashboardRegistry>()
        .expect("DashboardRegistry must exist after finish");
    let manifest = registry.manifest();
    assert_eq!(manifest.nav_items.len(), 2);
    assert_eq!(manifest.panels.len(), 2);

    // Mutating after freeze must not change the served snapshot.
    registry.add_nav_item(NavItem::new("late", "Late"));
    let again = registry.manifest();
    assert_eq!(again.nav_items.len(), 2);

    server.cleanup().await;
}
