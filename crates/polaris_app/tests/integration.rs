//! Integration tests for `polaris_app`.
//!
//! Tests the full plugin lifecycle: route registration, server startup,
//! HTTP request handling, and graceful shutdown.

use axum::Router;
use axum::routing::get;
use polaris_app::{AppConfig, AppPlugin, HttpRouter};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

/// A test plugin that registers a simple health check route.
struct TestRoutePlugin;

impl Plugin for TestRoutePlugin {
    const ID: &'static str = "test::routes";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let router = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/echo/{msg}", get(echo_handler));

        server
            .api::<HttpRouter>()
            .expect("HttpRouter must exist")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

async fn echo_handler(axum::extract::Path(msg): axum::extract::Path<String>) -> String {
    msg
}

/// A second plugin that registers routes independently.
struct AnotherRoutePlugin;

impl Plugin for AnotherRoutePlugin {
    const ID: &'static str = "test::another";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let router = Router::new().route("/ping", get(|| async { "pong" }));

        server
            .api::<HttpRouter>()
            .expect("HttpRouter must exist")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

/// Binds to an ephemeral port and returns the listener with its port.
///
/// The listener is kept alive and passed to [`AppPlugin::with_listener`] so
/// the port stays reserved (no TOCTOU race).
async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let port = listener
        .local_addr()
        .expect("failed to get local addr")
        .port();
    (listener, port)
}

/// Polls the server until it accepts a TCP connection, or panics after timeout.
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

#[tokio::test]
async fn plugin_registers_routes_and_server_responds() {
    let (listener, port) = bind_ephemeral().await;

    let mut server = Server::new();
    server.add_plugins(
        AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
            .with_listener(listener),
    );
    server.add_plugins(TestRoutePlugin);
    server.finish().await;

    wait_for_server(port).await;

    let base = format!("http://127.0.0.1:{port}");

    // Health check
    let resp = reqwest::get(format!("{base}/healthz"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");

    // Echo with path param
    let resp = reqwest::get(format!("{base}/echo/hello"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello");

    // 404 for unknown route
    let resp = reqwest::get(format!("{base}/unknown"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
async fn multiple_plugins_register_routes() {
    let (listener, port) = bind_ephemeral().await;

    let mut server = Server::new();
    server.add_plugins(
        AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
            .with_listener(listener),
    );
    server.add_plugins(TestRoutePlugin);
    server.add_plugins(AnotherRoutePlugin);
    server.finish().await;

    wait_for_server(port).await;

    let base = format!("http://127.0.0.1:{port}");

    // Routes from TestRoutePlugin
    let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Routes from AnotherRoutePlugin
    let resp = reqwest::get(format!("{base}/ping")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");

    server.cleanup().await;
}

#[tokio::test]
async fn request_id_header_is_present() {
    let (listener, port) = bind_ephemeral().await;

    let mut server = Server::new();
    server.add_plugins(
        AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
            .with_listener(listener),
    );
    server.add_plugins(TestRoutePlugin);
    server.finish().await;

    wait_for_server(port).await;

    let base = format!("http://127.0.0.1:{port}");
    let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();

    // x-request-id should be propagated to response
    assert!(
        resp.headers().contains_key("x-request-id"),
        "response should contain x-request-id header"
    );

    server.cleanup().await;
}
