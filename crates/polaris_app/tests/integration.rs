//! Integration tests for `polaris_app`.
//!
//! Tests the full plugin lifecycle: route registration, server startup,
//! HTTP request handling, and graceful shutdown.

use axum::Router;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use polaris_app::{AppConfig, AppPlugin, HttpRouter};
use polaris_core_plugins::{IOMessage, IOSource};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

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

// ─────────────────────────────────────────────────────────────────────────────
// SSE streaming tests
// ─────────────────────────────────────────────────────────────────────────────

/// Returns an SSE response with three pre-loaded IOMessages.
///
/// The sender is dropped immediately so the stream completes as soon as all
/// messages are consumed.
fn sse_event_name(source: &IOSource) -> &'static str {
    match source {
        IOSource::User => "user",
        IOSource::Agent(_) => "agent",
        IOSource::External(_) => "external",
        IOSource::System => "system",
    }
}

async fn sse_handler() -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<IOMessage>();
    tx.send(IOMessage::user_text("first")).unwrap();
    tx.send(IOMessage::system_text("second")).unwrap();
    tx.send(IOMessage::user_text("third")).unwrap();
    drop(tx);

    let stream = UnboundedReceiverStream::new(rx).map(|message| {
        Ok::<_, Infallible>(
            Event::default()
                .event(sse_event_name(&message.source))
                .json_data(&message)
                .unwrap_or_else(|json_err| {
                    Event::default().event("error").data(json_err.to_string())
                }),
        )
    });
    Sse::new(stream)
}

/// Plugin that registers the SSE test endpoint.
struct SseTestPlugin;

impl Plugin for SseTestPlugin {
    const ID: &'static str = "test::sse";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        let router = Router::new().route("/sse", get(sse_handler));
        server
            .api::<HttpRouter>()
            .expect("HttpRouter must exist")
            .add_routes(router);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
    }
}

/// Parses raw SSE text into a list of `(event_type, data)` pairs.
fn parse_sse_frames(body: &str) -> Vec<(String, String)> {
    let mut frames = Vec::new();
    for block in body.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut event_type = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            } else if let Some(value) = line.strip_prefix("event:") {
                event_type = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data = value.trim().to_string();
            }
        }
        if !event_type.is_empty() || !data.is_empty() {
            frames.push((event_type, data));
        }
    }
    frames
}

#[tokio::test]
async fn sse_streams_io_messages() {
    let (listener, port) = bind_ephemeral().await;

    let mut server = Server::new();
    server.add_plugins(
        AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
            .with_listener(listener),
    );
    server.add_plugins(SseTestPlugin);
    server.finish().await;

    wait_for_server(port).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/sse"))
        .await
        .expect("SSE request failed");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );

    let body = resp.text().await.expect("failed to read SSE body");
    let frames = parse_sse_frames(&body);

    assert_eq!(frames.len(), 3, "expected 3 SSE events, got: {frames:?}");

    assert_eq!(frames[0].0, "user");
    let data0: serde_json::Value =
        serde_json::from_str(&frames[0].1).expect("frame 0 data is not valid JSON");
    assert_eq!(data0["content"]["Text"], "first");
    assert_eq!(data0["source"], "User");

    assert_eq!(frames[1].0, "system");
    let data1: serde_json::Value =
        serde_json::from_str(&frames[1].1).expect("frame 1 data is not valid JSON");
    assert_eq!(data1["content"]["Text"], "second");
    assert_eq!(data1["source"], "System");

    assert_eq!(frames[2].0, "user");
    let data2: serde_json::Value =
        serde_json::from_str(&frames[2].1).expect("frame 2 data is not valid JSON");
    assert_eq!(data2["content"]["Text"], "third");
    assert_eq!(data2["source"], "User");

    server.cleanup().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket integration tests (gated behind the `ws` feature)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(feature = "ws")]
mod ws_tests {
    use super::*;
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use futures_util::{SinkExt, StreamExt};
    use polaris_app::WsRouter;

    async fn ws_echo_upgrade(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.on_upgrade(ws_echo_handler)
    }

    async fn ws_echo_handler(mut socket: WebSocket) {
        while let Some(Ok(msg)) = socket.next().await {
            match msg {
                Message::Text(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    }

    struct WsEchoPlugin;

    impl Plugin for WsEchoPlugin {
        const ID: &'static str = "test::ws_echo";
        const VERSION: Version = Version::new(0, 0, 1);

        fn build(&self, server: &mut Server) {
            let router = Router::new().route("/ws/echo", get(ws_echo_upgrade));

            server
                .api::<WsRouter>()
                .expect("WsRouter must exist (ws feature enabled)")
                .add_routes(router);
        }

        fn dependencies(&self) -> Vec<PluginId> {
            vec![PluginId::of::<AppPlugin>()]
        }
    }

    #[tokio::test]
    async fn ws_echo_round_trip() {
        let (listener, port) = bind_ephemeral().await;

        let mut server = Server::new();
        server.add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        );
        server.add_plugins(WsEchoPlugin);
        server.finish().await;

        wait_for_server(port).await;

        let url = format!("ws://127.0.0.1:{port}/ws/echo");
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("WebSocket connection failed");

        let (mut write, mut read) = ws_stream.split();

        write
            .send(tokio_tungstenite::tungstenite::Message::Text(
                "hello ws".into(),
            ))
            .await
            .expect("failed to send WS message");

        let response = read
            .next()
            .await
            .expect("stream ended unexpectedly")
            .expect("failed to read WS message");

        assert_eq!(
            response.into_text().expect("expected text frame"),
            "hello ws"
        );

        write
            .send(tokio_tungstenite::tungstenite::Message::Close(None))
            .await
            .expect("failed to send close frame");

        server.cleanup().await;
    }

    #[tokio::test]
    async fn ws_upgrade_rejected_by_auth() {
        use axum::response::Response;
        use http::StatusCode;
        use polaris_app::auth::AuthRejection;

        #[derive(Debug)]
        struct RejectAllAuth;

        impl polaris_app::AuthProvider for RejectAllAuth {
            fn authenticate(&self, _parts: &http::request::Parts) -> Result<(), AuthRejection> {
                Err(Box::new(
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(axum::body::Body::from("unauthorized"))
                        .expect("failed to build rejection response"),
                ))
            }
        }

        struct AuthWsPlugin;

        impl Plugin for AuthWsPlugin {
            const ID: &'static str = "test::auth_ws";
            const VERSION: Version = Version::new(0, 0, 1);

            fn build(&self, server: &mut Server) {
                let http_router = server.api::<HttpRouter>().expect("HttpRouter must exist");
                http_router.set_auth(RejectAllAuth);

                let ws_router = server
                    .api::<WsRouter>()
                    .expect("WsRouter must exist (ws feature enabled)");
                ws_router.add_routes(Router::new().route("/ws/echo", get(ws_echo_upgrade)));
            }

            fn dependencies(&self) -> Vec<PluginId> {
                vec![PluginId::of::<AppPlugin>()]
            }
        }

        let (listener, port) = bind_ephemeral().await;

        let mut server = Server::new();
        server.add_plugins(
            AppPlugin::new(
                AppConfig::new()
                    .with_host("127.0.0.1")
                    .with_port(port)
                    // Auth + no explicit origins now panics; opt into wildcard
                    // CORS for this test since it only exercises auth rejection.
                    .with_allow_any_cors_origin(),
            )
            .with_listener(listener),
        );
        server.add_plugins(AuthWsPlugin);
        server.finish().await;

        wait_for_server(port).await;

        let url = format!("ws://127.0.0.1:{port}/ws/echo");
        let result = tokio_tungstenite::connect_async(&url).await;

        assert!(
            result.is_err(),
            "WebSocket upgrade should fail when auth rejects"
        );

        server.cleanup().await;
    }
}
