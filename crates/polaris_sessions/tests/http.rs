//! Integration tests for the session HTTP endpoints.
//!
//! Requires the `http` feature. Verifies CRUD operations, error responses,
//! and status codes for the session REST API.

#![cfg(feature = "http")]

use polaris_agent::Agent;
use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::PersistencePlugin;
use polaris_graph::graph::Graph;
use polaris_sessions::http::HttpPlugin;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::{SessionsAPI, SessionsPlugin};
use polaris_system::server::Server;
use polaris_system::system;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn noop() {}

struct NoOpAgent;

impl Agent for NoOpAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(noop);
    }

    fn name(&self) -> &'static str {
        "NoOpAgent"
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

/// Builds a server with `AppPlugin`, `SessionsPlugin`, and `HttpPlugin` on the given port.
async fn test_server(listener: tokio::net::TcpListener, port: u16) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(HttpPlugin::new());
    server.finish().await;

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(NoOpAgent).unwrap();

    server
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_session_returns_201() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/sessions"))
        .json(&serde_json::json!({ "agent_type": "NoOpAgent" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["agent_type"], "NoOpAgent");
    assert!(body["session_id"].is_string());
    assert_eq!(body["turn_number"], 0);
    assert!(body["created_at"].is_string());
    assert_eq!(body["status"], "active");

    server.cleanup().await;
}

#[tokio::test]
async fn create_session_with_custom_id() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/sessions"))
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "my-session"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["session_id"], "my-session");

    server.cleanup().await;
}

#[tokio::test]
async fn create_session_unknown_agent_returns_400() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/sessions"))
        .json(&serde_json::json!({ "agent_type": "DoesNotExist" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "agent_not_found");

    server.cleanup().await;
}

#[tokio::test]
async fn list_sessions_returns_empty() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/v1/sessions"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessions"], serde_json::json!([]));

    server.cleanup().await;
}

#[tokio::test]
async fn create_duplicate_session_returns_409() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/sessions");

    // First create succeeds.
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "dup-test"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Second create with same ID returns 409 Conflict.
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "dup-test"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "session_already_exists");

    server.cleanup().await;
}

#[tokio::test]
async fn list_sessions_after_create() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // Create a session
    client
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "sess-1"
        }))
        .send()
        .await
        .unwrap();

    // List should contain 1 session
    let resp = client
        .get(format!("{base}/v1/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["agent_type"], "NoOpAgent");
    assert!(sessions[0]["created_at"].is_string());
    assert_eq!(sessions[0]["status"], "active");

    server.cleanup().await;
}

#[tokio::test]
async fn get_session_returns_metadata() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // Create
    client
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "meta-test"
        }))
        .send()
        .await
        .unwrap();

    // Get
    let resp = client
        .get(format!("{base}/v1/sessions/meta-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["agent_type"], "NoOpAgent");
    assert_eq!(body["turn_number"], 0);
    assert!(body["created_at"].is_string());
    assert_eq!(body["status"], "active");

    server.cleanup().await;
}

#[tokio::test]
async fn get_session_not_found_returns_404() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::get(format!("http://127.0.0.1:{port}/v1/sessions/nonexistent"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "session_not_found");

    server.cleanup().await;
}

#[tokio::test]
async fn delete_session_returns_204() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // Create
    client
        .post(format!("{base}/v1/sessions"))
        .json(&serde_json::json!({
            "agent_type": "NoOpAgent",
            "session_id": "del-test"
        }))
        .send()
        .await
        .unwrap();

    // Delete
    let resp = client
        .delete(format!("{base}/v1/sessions/del-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Get should now 404
    let resp = client
        .get(format!("{base}/v1/sessions/del-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
async fn delete_nonexistent_session_is_idempotent() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    // Delete is idempotent — deleting a non-existent session returns 204.
    let resp = reqwest::Client::new()
        .delete(format!("http://127.0.0.1:{port}/v1/sessions/nonexistent"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 204);

    server.cleanup().await;
}
