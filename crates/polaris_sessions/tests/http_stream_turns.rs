//! Integration tests for the per-turn SSE streaming endpoint.
//!
//! Verifies `POST /v1/sessions/{id}/turns/stream`: SSE event
//! delivery, terminal events, pre-stream error handling, and
//! concurrency semantics.

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
use polaris_system::system::SystemError;
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// System that echoes user input with an "echo: " prefix.
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

/// System that blocks on a second receive, holding the context lock.
///
/// Emits a `system` `IOMessage` after the first receive so callers can
/// synchronize on lock acquisition without sleeping on the wall clock.
#[system]
async fn blocking_receive(io: Res<UserIO>) {
    let _first = io.receive().await.expect("should receive first message");
    io.send(IOMessage::system_text("blocking-ready"))
        .await
        .expect("should signal lock acquisition");
    let _second = io.receive().await;
}

struct BlockingAgent;

impl Agent for BlockingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(blocking_receive);
    }

    fn name(&self) -> &'static str {
        "BlockingAgent"
    }
}

/// System that fails with a non-busy execution error mid-turn.
#[system]
async fn always_fail() -> Result<(), SystemError> {
    Err(SystemError::ExecutionError(
        "intentional turn failure".into(),
    ))
}

struct FailingAgent;

impl Agent for FailingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(always_fail);
    }

    fn name(&self) -> &'static str {
        "FailingAgent"
    }
}

/// System that emits one output message and *then* fails mid-turn, so the
/// turn record exists with partial IO when the execution error propagates.
#[system]
async fn emit_then_fail(io: Res<UserIO>) -> Result<(), SystemError> {
    let _ = io.receive().await.expect("should receive a message");
    io.send(IOMessage::system_text("partial output before failure"))
        .await
        .expect("should send partial output");
    Err(SystemError::ExecutionError(
        "intentional failure after emitting output".into(),
    ))
}

struct EmitThenFailAgent;

impl Agent for EmitThenFailAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(emit_then_fail);
    }

    fn name(&self) -> &'static str {
        "EmitThenFailAgent"
    }
}

/// Binds to an ephemeral port and returns the listener with its port.
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

/// Builds a test server with the full HTTP stack.
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
    sessions.register_agent(BlockingAgent).unwrap();
    sessions.register_agent(FailingAgent).unwrap();
    sessions.register_agent(EmitThenFailAgent).unwrap();

    server
}

/// Creates a session via the REST API and returns the session ID.
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

/// Parsed SSE frame.
#[derive(Debug)]
struct SseFrame {
    event: String,
    data: String,
}

/// Parses SSE frames from raw `text/event-stream` body text.
fn parse_sse_frames(body: &str) -> Vec<SseFrame> {
    let mut frames = Vec::new();
    let mut event = String::new();
    let mut data_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_owned();
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_owned());
        } else if line.is_empty() && (!event.is_empty() || !data_lines.is_empty()) {
            frames.push(SseFrame {
                event: std::mem::take(&mut event),
                data: data_lines.join("\n"),
            });
            data_lines.clear();
        }
    }
    // Flush any trailing frame without a final blank line.
    if !event.is_empty() || !data_lines.is_empty() {
        frames.push(SseFrame {
            event,
            data: data_lines.join("\n"),
        });
    }
    frames
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Happy path: SSE stream delivers `IOMessage` events and a terminal `done` event.
#[tokio::test]
async fn stream_turn_echo() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let session_id = create_session(port, "EchoAgent").await;

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id}/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got: {content_type}"
    );

    let body = resp.text().await.unwrap();
    let frames = parse_sse_frames(&body);

    // Should have at least one IOMessage event and a terminal `done` event.
    assert!(
        frames.len() >= 2,
        "expected at least 2 SSE frames, got {}: {frames:?}",
        frames.len()
    );

    // Find the system message (echo response).
    let system_frame = frames
        .iter()
        .find(|f| f.event == "system")
        .expect("expected a 'system' event");
    let data: serde_json::Value = serde_json::from_str(&system_frame.data).unwrap();
    assert!(
        data["content"]["Text"]
            .as_str()
            .unwrap()
            .contains("echo: hello"),
        "expected echo response, got: {data}"
    );

    // Last non-empty frame should be `done`.
    let done_frame = frames.last().expect("expected at least one frame");
    assert_eq!(done_frame.event, "done", "last frame should be 'done'");
    let done_data: serde_json::Value = serde_json::from_str(&done_frame.data).unwrap();
    assert!(
        done_data["execution"]["nodes_executed"].as_u64().unwrap() > 0,
        "expected nodes_executed > 0"
    );
    assert_eq!(done_data["execution"]["turn_number"], 1);

    server.cleanup().await;
}

/// Pre-stream error: unknown session returns HTTP 404, not SSE.
#[tokio::test]
async fn stream_turn_not_found() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/nonexistent/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "session_not_found");

    server.cleanup().await;
}

/// Concurrent turn on a busy session surfaces the error through an SSE event.
#[tokio::test]
async fn stream_turn_busy() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let session_id_str = create_session(port, "BlockingAgent").await;

    // Start a blocking turn via the API directly, keeping input_tx alive
    // so the BlockingAgent's second receive() blocks indefinitely.
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let sid = polaris_sessions::store::SessionId::from_string(session_id_str.clone());

    let (provider, input_tx, mut output_rx) = polaris_sessions::http::HttpIOProvider::new(32, 32);
    let provider = Arc::new(provider);
    input_tx.send(IOMessage::user_text("block")).await.unwrap();
    // Do NOT drop input_tx — keep channel alive so BlockingAgent blocks.

    let io_provider = Arc::clone(&provider);
    let sessions_clone = sessions.clone();
    let sid_clone = sid.clone();
    let blocking_handle = tokio::spawn(async move {
        sessions_clone
            .try_process_turn_with(&sid_clone, move |ctx| {
                ctx.insert(UserIO::new(io_provider));
            })
            .await
    });

    // Synchronize on the BlockingAgent's `blocking-ready` system message
    // so we know the session lock is held before we issue the streaming
    // request. Bounded by a deadline to avoid hanging forever on a bug.
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), output_rx.recv())
        .await
        .expect("BlockingAgent did not signal lock acquisition within 5 s")
        .expect("output channel closed before ready signal");
    assert!(
        matches!(ready.content, IOContent::Text(ref t) if t == "blocking-ready"),
        "expected blocking-ready signal, got {ready:?}"
    );

    // The streaming turn should see a session_busy error. Because the
    // session exists (pre-stream validation passes), the error surfaces
    // as an SSE error event, not an HTTP error.
    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id_str}/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "hello" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let frames = parse_sse_frames(&body);

    let error_frame = frames
        .iter()
        .find(|f| f.event == "error")
        .unwrap_or_else(|| panic!("expected an 'error' SSE event, got frames: {frames:?}"));
    let error_json: serde_json::Value =
        serde_json::from_str(&error_frame.data).unwrap_or_else(|json_err| {
            panic!(
                "error frame must be valid JSON ({json_err}), got: {}",
                error_frame.data
            )
        });
    assert_eq!(error_json["code"], "session_busy");
    assert!(
        error_json["message"].is_string(),
        "error frame must have string message, got: {error_json}"
    );

    // Clean up: drop input_tx to unblock the BlockingAgent so the spawned
    // turn returns naturally; then await the join handle (no abort).
    drop(input_tx);
    let _ = blocking_handle.await;
    server.cleanup().await;
}

/// Regression: a streaming turn that loses the session race must not
/// overwrite the in-flight turn's recorded messages with an empty `Vec`.
///
/// Two SSE clients on one session share the same `turn_before` (neither has
/// completed, so the turn counter has not advanced). The winner records the
/// turn's IO messages on success; the loser gets `SessionBusy`, captures
/// nothing, and — before the fix — unconditionally called
/// `record_turn_messages` with an empty `Vec`, wiping the winner's history.
///
/// The race is made deterministic by holding the session lock with a
/// blocking turn (so the streaming request is guaranteed to lose) and by
/// seeding turn 0's record up front to stand in for the winner's commit.
/// Draining the loser's full SSE body guarantees its spawned task ran the
/// record step, which precedes the terminal event.
#[tokio::test]
async fn stream_turn_busy_does_not_clobber_recorded_messages() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let session_id_str = create_session(port, "BlockingAgent").await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let sid = polaris_sessions::store::SessionId::from_string(session_id_str.clone());

    // Hold the session lock with a blocking turn (turn 0). While it is in
    // flight the turn counter stays at 0, so the racing streaming request
    // below captures `turn_before == 0` — the same turn we seed.
    let (provider, input_tx, mut output_rx) = polaris_sessions::http::HttpIOProvider::new(32, 32);
    let provider = Arc::new(provider);
    input_tx.send(IOMessage::user_text("block")).await.unwrap();

    let io_provider = Arc::clone(&provider);
    let sessions_clone = sessions.clone();
    let sid_clone = sid.clone();
    let blocking_handle = tokio::spawn(async move {
        sessions_clone
            .try_process_turn_with(&sid_clone, move |ctx| {
                ctx.insert(UserIO::new(io_provider));
            })
            .await
    });

    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), output_rx.recv())
        .await
        .expect("BlockingAgent did not signal lock acquisition within 5 s")
        .expect("output channel closed before ready signal");
    assert!(
        matches!(ready.content, IOContent::Text(ref t) if t == "blocking-ready"),
        "expected blocking-ready signal, got {ready:?}"
    );

    // Stand in for the winner's success path: record turn 0's IO messages,
    // exactly as `process_turn_stream` does on completion.
    let winner_messages = vec![
        IOMessage::user_text("hello"),
        IOMessage::system_text("echo: hello"),
    ];
    sessions.record_turn_messages(&sid, 0, winner_messages.clone());
    let seeded = serde_json::to_value(&winner_messages).unwrap();
    assert_eq!(
        serde_json::to_value(sessions.turn(&sid, 0).expect("turn 0 should exist").messages)
            .unwrap(),
        seeded,
        "winner's messages should be recorded before the racing request"
    );

    // The losing streaming request: the session is busy, so it returns a
    // `session_busy` SSE error rather than executing a turn.
    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id_str}/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "loser" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let frames = parse_sse_frames(&body);
    let error_frame = frames
        .iter()
        .find(|f| f.event == "error")
        .unwrap_or_else(|| panic!("expected an 'error' SSE event, got frames: {frames:?}"));
    let error_json: serde_json::Value = serde_json::from_str(&error_frame.data).unwrap();
    assert_eq!(error_json["code"], "session_busy");

    // The winner's recorded messages must survive untouched — the loser
    // must not have overwritten them with an empty `Vec`.
    assert_eq!(
        serde_json::to_value(
            sessions
                .turn(&sid, 0)
                .expect("turn 0 should still exist")
                .messages
        )
        .unwrap(),
        seeded,
        "a SessionBusy streaming request must not clobber recorded turn messages"
    );

    // Clean up: unblock the BlockingAgent so the spawned turn returns.
    drop(input_tx);
    let _ = blocking_handle.await;
    server.cleanup().await;
}

/// Mid-turn `SessionError` other than `SessionBusy` surfaces through the
/// SSE error event path. Covers the general `Err(session_err)` mapping at
/// `handlers.rs::process_turn_stream` for non-busy execution failures.
#[tokio::test]
async fn stream_turn_internal_error_emits_error_event() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let session_id = create_session(port, "FailingAgent").await;

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id}/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "ignored" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let frames = parse_sse_frames(&body);

    let error_frame = frames
        .iter()
        .find(|f| f.event == "error")
        .unwrap_or_else(|| panic!("expected an 'error' SSE event, got frames: {frames:?}"));
    let error_json: serde_json::Value = serde_json::from_str(&error_frame.data)
        .unwrap_or_else(|json_err| panic!("error frame must be valid JSON ({json_err})"));
    // The `FailingAgent` system returns `SystemError::ExecutionError`,
    // which surfaces through `SessionError::Execution` and maps to the
    // generic `internal_error` ApiError variant — explicitly *not*
    // `session_busy`. The detail message is not surfaced to clients;
    // it is logged server-side via `tracing::error!`.
    assert_eq!(error_json["code"], "internal_error");
    let msg = error_json["message"]
        .as_str()
        .expect("error frame must have string message");
    assert_eq!(
        msg, "internal server error",
        "internal-error message must not leak server-side detail, got: {msg}"
    );

    // No `done` frame should follow an error frame.
    assert!(
        frames.last().map(|f| f.event.as_str()) == Some("error"),
        "error event should be the terminal frame, got: {frames:?}"
    );

    server.cleanup().await;
}

/// A turn that *executed* but failed mid-run must still record the IO that
/// flowed before the failure — `record_turn_messages` is gated on whether
/// the task executed the turn, not on success. This keeps the recorded
/// messages faithful to the turn's `Failed` status, in contrast to a
/// `SessionBusy` loser (which never executed and must record nothing).
#[tokio::test]
async fn stream_turn_failed_after_emitting_records_partial_messages() {
    let (listener, port) = bind_ephemeral().await;
    let mut server = test_server(listener, port).await;
    wait_for_server(port).await;

    let session_id_str = create_session(port, "EmitThenFailAgent").await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let sid = polaris_sessions::store::SessionId::from_string(session_id_str.clone());

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{port}/v1/sessions/{session_id_str}/turns/stream"
        ))
        .json(&serde_json::json!({ "message": "go" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Drain the full SSE body so the spawned task's record step (which
    // precedes the terminal event) is guaranteed to have run.
    let body = resp.text().await.unwrap();
    let frames = parse_sse_frames(&body);
    assert!(
        frames.iter().any(|f| f.event == "error"),
        "an executed-but-failed turn must still surface an error frame, got: {frames:?}"
    );

    // The partial output emitted before the failure must be recorded
    // against turn 0, not dropped.
    let turn = sessions.turn(&sid, 0).expect("turn 0 should exist");
    let texts: Vec<_> = turn
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            IOContent::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().any(|t| t == "partial output before failure"),
        "a failed-but-executed turn must record the IO emitted before failure, got: {texts:?}"
    );

    server.cleanup().await;
}
