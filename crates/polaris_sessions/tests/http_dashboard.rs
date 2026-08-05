//! Integration tests for the A9 dashboard HTTP endpoints:
//!
//! - `GET /v1/sessions/agent-types`
//! - `GET /v1/sessions/{id}/turns` (with optional `?include=messages`)
//! - `GET /v1/sessions/{id}/turns/{n}`
//! - `GET /v1/sessions/{id}/uptime`

#![cfg(feature = "sessions-http")]

use axum::http::{StatusCode, request::Parts};
use polaris_agent::Agent;
use polaris_app::auth::AuthRejection;
use polaris_app::{AppConfig, AppPlugin, AuthProvider, HttpRouter};
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_core_plugins::{IOContent, IOMessage, UserIO};
use polaris_graph::GraphSignature;
use polaris_graph::graph::Graph;
use polaris_sessions::http::HttpPlugin;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::{SessionsAPI, SessionsPlugin};
use polaris_system::param::Res;
use polaris_system::plugin::{Plugin, PluginId, Version};
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

#[derive(Debug)]
struct HeaderAuth;

impl AuthProvider for HeaderAuth {
    fn authenticate(&self, parts: &Parts) -> Result<(), AuthRejection> {
        let authorized = parts
            .headers
            .get("x-test-auth")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "ok");

        if authorized {
            Ok(())
        } else {
            Err(Box::new(
                axum::response::Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(axum::body::Body::from("unauthorized"))
                    .expect("failed to build rejection response"),
            ))
        }
    }
}

struct TestAuthPlugin;

impl Plugin for TestAuthPlugin {
    const ID: &'static str = "test::sessions_http_auth";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server
            .api::<HttpRouter>()
            .expect("HttpRouter must exist")
            .set_auth(HeaderAuth);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<AppPlugin>()]
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
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(EchoAgent).unwrap();
    server
}

async fn test_server_with_auth(listener: tokio::net::TcpListener, port: u16) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())).without_auto_checkpoint())
        .add_plugins(
            AppPlugin::new(
                AppConfig::new()
                    .with_host("127.0.0.1")
                    .with_port(port)
                    .with_allow_any_cors_origin()
                    .with_public_path("/v1/sessions/agent-types"),
            )
            .with_listener(listener),
        )
        .add_plugins(TestAuthPlugin)
        .add_plugins(HttpPlugin::new());
    server.finish().await.unwrap();

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

    assert!(
        items[0].get("signature").is_none(),
        "public agent type listing must not expose graph signatures"
    );
    assert!(
        items[0].get("contracts").is_none(),
        "public agent type listing must not expose capability contracts"
    );

    let details = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/agent-types/details"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        details.status(),
        404,
        "private metadata route is not mounted without an AuthProvider"
    );

    server.cleanup().await;
}

#[tokio::test]
async fn list_agent_types_advertises_satisfied_contracts() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server_with_auth(listener, port).await;

    let sessions = server.api::<SessionsAPI>().unwrap();
    // EchoAgent reads UserIO and produces nothing, so it satisfies the
    // io-reader slot but not the one demanding a produced output.
    sessions
        .register_contract("echo-io", GraphSignature::new().require_read::<UserIO>())
        .unwrap();
    sessions
        .register_contract("unrelated", GraphSignature::new().produce::<u32>())
        .unwrap();
    wait_for_server(port).await;

    let client = reqwest::Client::new();
    let denied = client
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/agent-types/details"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        401,
        "private agent type metadata must require authentication"
    );

    let resp = client
        .get(format!(
            "http://127.0.0.1:{port}/v1/sessions/agent-types/details"
        ))
        .header("x-test-auth", "ok")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items[0]["name"], "EchoAgent");
    let requires = items[0]["signature"]["requires"]
        .as_array()
        .expect("signature.requires array");
    assert!(
        requires
            .iter()
            .any(|entry| entry.as_str().is_some_and(|s| s.contains("UserIO"))),
        "rendered requires should mention the UserIO read: {requires:?}"
    );
    assert_eq!(
        items[0]["signature"]["requires_outputs"],
        serde_json::json!([])
    );
    assert_eq!(items[0]["signature"]["produces"], serde_json::json!([]));
    assert_eq!(items[0]["contracts"], serde_json::json!(["echo-io"]));

    server.cleanup().await;
}

/// The reuse contract of `routes()`: the router is mount-point-relative, so
/// a consumer (e.g. a dashboard crate) can nest the canonical endpoints
/// under its own prefix without `HttpPlugin`.
#[tokio::test]
async fn routes_nest_under_a_custom_prefix() {
    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use tower::ServiceExt;

    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    sessions.register_agent(EchoAgent).unwrap();
    let app = axum::Router::new().nest("/api", polaris_sessions::http::routes(sessions.clone()));

    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sessions/agent-types")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(ok.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["items"][0]["name"], "EchoAgent");
    assert!(body["items"][0].get("signature").is_none());
    assert!(body["items"][0].get("contracts").is_none());

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/sessions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"agent_type":"EchoAgent"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(created.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body["session_id"].as_str().is_some(),
        "standalone routes should create a session when SessionsAPI is fully wired"
    );

    let private_metadata = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sessions/agent-types/details")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(private_metadata.status(), StatusCode::NOT_FOUND);

    // The un-prefixed path must not resolve — nesting didn't flatten the
    // routes onto the root.
    let missed = app
        .oneshot(
            Request::builder()
                .uri("/v1/sessions/agent-types")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missed.status(), StatusCode::NOT_FOUND);

    server.cleanup().await;
}

/// The enriched fields are additive: a response from a server predating
/// signature advertisement must still deserialize into the new model.
#[test]
fn agent_type_summary_deserializes_pre_signature_responses() {
    use polaris_sessions::http::models::AgentTypeSummary;

    let summary: AgentTypeSummary = serde_json::from_str(r#"{"name":"EchoAgent"}"#).unwrap();
    assert_eq!(summary.name, "EchoAgent");
    assert!(summary.signature.is_none());
    assert!(summary.contracts.is_empty());
}

#[test]
fn turn_models_deserialize_pre_truncation_responses() {
    use polaris_sessions::http::models::{Turn, TurnSummary};

    let summary: TurnSummary = serde_json::from_str(
        r#"{
            "turn": 0,
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": null,
            "status": "completed",
            "io_message_count": 0,
            "last_message_preview": null
        }"#,
    )
    .unwrap();
    assert!(!summary.messages_truncated);

    let turn: Turn = serde_json::from_str(
        r#"{
            "turn": 0,
            "started_at": "2026-01-01T00:00:00Z",
            "finished_at": null,
            "status": "completed",
            "messages": []
        }"#,
    )
    .unwrap();
    assert!(!turn.messages_truncated);
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
    assert_eq!(entry["messages_truncated"], false);
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
async fn turn_history_caps_retained_messages_and_payload_bytes() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let id = create_session(port, "EchoAgent").await;
    run_turn(port, &id, "seed").await;
    let session_id = polaris_sessions::store::SessionId::from_string(id);
    let sessions = server.api::<SessionsAPI>().unwrap();

    let messages = (0..300)
        .map(|index| IOMessage::system_text(format!("message-{index}")))
        .collect();
    sessions.record_turn_messages(&session_id, 0, messages);

    let summary = sessions.turn_history(&session_id, true).unwrap().remove(0);
    assert_eq!(summary.io_message_count, 300);
    assert_eq!(summary.messages.as_ref().unwrap().len(), 256);
    assert!(summary.messages_truncated);
    assert_eq!(summary.last_message_preview.as_deref(), Some("message-299"));

    sessions.record_turn_messages(
        &session_id,
        0,
        vec![IOMessage::system_text("x".repeat(128 * 1024))],
    );
    let turn = sessions.turn(&session_id, 0).unwrap();
    assert!(turn.messages_truncated);
    let IOContent::Text(text) = &turn.messages[0].content else {
        panic!("expected retained text message");
    };
    assert!(text.len() <= 64 * 1024);

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
