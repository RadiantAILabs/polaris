//! Integration tests for the A9 dashboard HTTP endpoints:
//!
//! - `GET /v1/sessions/agent-types`
//! - `GET /v1/sessions/{id}/turns` (with optional `?include=messages`)
//! - `GET /v1/sessions/{id}/turns/{n}`
//! - `GET /v1/sessions/{id}/uptime`

#![cfg(feature = "sessions-http")]

use polaris_agent::Agent;
use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_core_plugins::{IOContent, IOMessage, UserIO};
use polaris_graph::graph::Graph;
use polaris_sessions::http::HttpPlugin;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::{SessionsAPI, SessionsPlugin};
use polaris_system::param::Res;
use polaris_system::server::Server;
use polaris_system::system;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

#[system]
async fn echo(io: Res<UserIO>) {
    let msg = io.receive().await.expect("should receive a message");
    let text = match msg.content {
        IOContent::Text(ref text) => text.clone(),
        _ => String::from("non-text"),
    };
    io.send(IOMessage::system_text(format!("echo: {text}")))
        .await
        .expect("should send response");
}

struct EchoAgent;

impl Agent for EchoAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(echo);
    }

    fn name(&self) -> &'static str {
        "EchoAgent"
    }
}

async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
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

async fn test_server(listener: tokio::net::TcpListener, port: u16) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())).without_auto_checkpoint())
        .add_plugins(
            AppPlugin::new(AppConfig::new().with_host("127.0.0.1").with_port(port))
                .with_listener(listener),
        )
        .add_plugins(HttpPlugin::new());
    server.finish().await;

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(EchoAgent).unwrap();
    server
}

async fn create_session(port: u16, agent: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/sessions"))
        .json(&serde_json::json!({ "agent_type": agent }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["session_id"].as_str().unwrap().to_owned()
}

async fn run_turn(port: u16, id: &str, message: &str) {
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/sessions/{id}/turns"))
        .json(&serde_json::json!({ "message": message }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "turn should succeed: {:?}",
        resp.text().await
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Agent types
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_agent_types_returns_registered_agents() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/agent-types"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "EchoAgent");

    server.cleanup().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Turn history
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_turns_returns_summaries_after_turn() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    run_turn(port, &id, "hello").await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/{id}/turns"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);

    let entry = &items[0];
    assert_eq!(entry["turn"], 0);
    assert_eq!(entry["status"], "completed");
    assert!(entry["started_at"].is_string());
    assert!(entry["finished_at"].is_string());
    // EchoAgent emits exactly one system message per turn.
    assert_eq!(entry["io_message_count"], 1);
    assert!(
        entry["last_message_preview"]
            .as_str()
            .unwrap()
            .contains("echo: hello"),
        "preview should reflect the last system message: {entry}"
    );
    // Without `?include=messages`, the embedded array is omitted entirely.
    assert!(entry.get("messages").is_none());

    server.cleanup().await;
}

#[tokio::test]
async fn list_turns_with_include_messages_embeds_io_messages() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    run_turn(port, &id, "hi").await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{id}/turns?include=messages"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let messages = body["items"][0]["messages"]
        .as_array()
        .expect("messages array when include=messages is set");
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]["content"]["Text"]
            .as_str()
            .unwrap()
            .contains("echo: hi")
    );

    server.cleanup().await;
}

#[tokio::test]
async fn list_turns_unknown_session_returns_404() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/missing/turns"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "session_not_found");

    server.cleanup().await;
}

#[tokio::test]
async fn get_turn_returns_full_payload() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    run_turn(port, &id, "detail").await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/{id}/turns/0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["turn"], 0);
    assert_eq!(body["status"], "completed");
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]["content"]["Text"]
            .as_str()
            .unwrap()
            .contains("echo: detail")
    );

    server.cleanup().await;
}

#[tokio::test]
async fn get_turn_unknown_turn_returns_400() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;

    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/{id}/turns/99"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "turn_not_found");

    server.cleanup().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Uptime
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn uptime_returns_buckets_for_live_session() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    run_turn(port, &id, "warmup").await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{id}/uptime?bucket=1m"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["bucket"], "1m");
    assert!(body["since"].is_string());
    assert!(body["until"].is_string());
    let buckets = body["buckets"].as_array().expect("buckets array");
    // 24h default range / 1m bucket = 1440 buckets.
    assert_eq!(buckets.len(), 24 * 60);
    // The most recent bucket should be Active (turn just ran), but allow
    // it to land in either of the last two buckets to avoid races on
    // bucket-edge timing.
    let last_two: Vec<&str> = buckets[buckets.len() - 2..]
        .iter()
        .map(|b| b["status"].as_str().unwrap())
        .collect();
    assert!(
        last_two.contains(&"active"),
        "expected an active bucket near the end of the series, got tail: {last_two:?}"
    );

    server.cleanup().await;
}

#[tokio::test]
async fn uptime_rejects_unknown_bucket_with_400() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{id}/uptime?bucket=30s"
        ))
        .send()
        .await
        .unwrap();
    // axum's Query<T> rejection surfaces as 400 on deserialization
    // failure — which is exactly the contract here.
    assert_eq!(resp.status(), 400);

    server.cleanup().await;
}

#[tokio::test]
async fn uptime_rejects_oversize_window_with_400() {
    // A 100-year `since` against the 1m default would request ~5.3e7
    // buckets, which used to allocate a vec of that size before responding.
    // The handler now rejects with `bad_request` before reaching the
    // recorder.
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{id}/uptime?since=1925-01-01T00:00:00Z&until=2025-01-01T00:00:00Z&bucket=1m"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "bad_request");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("buckets") && message.contains("limit"),
        "expected message to call out the bucket limit, got: {message}"
    );

    server.cleanup().await;
}

#[tokio::test]
async fn uptime_rejects_malformed_timestamp_with_400() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/{id}/uptime?since=not-a-timestamp"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "bad_request");

    server.cleanup().await;
}

#[tokio::test]
async fn uptime_unknown_session_returns_404() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/missing/uptime"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "session_not_found");

    server.cleanup().await;
}

#[tokio::test]
async fn uptime_after_delete_returns_404() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    let del = reqwest::Client::new()
        .delete(format!("http://127.0.0.1:{port}/v1/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 204);

    // After delete, the session is gone — uptime endpoint 404s rather
    // than returning a frozen "terminated" series.
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/sessions/{id}/uptime"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    server.cleanup().await;
}
