//! Integration tests for the turn processing endpoint.
//!
//! Verifies IO bridging via [`HttpIOProvider`]: user messages reach the
//! agent, agent responses are collected, and concurrency is handled.

#![cfg(feature = "http")]

use polaris_agent::Agent;
use polaris_app::HttpIOProvider;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin};
use polaris_core_plugins::{IOContent, IOMessage, UserIO};
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::Res;
use polaris_system::server::Server;
use polaris_system::system;
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

/// System that calls receive twice — the second will block forever,
/// holding the context lock.
#[system]
async fn blocking_receive(io: Res<UserIO>) {
    let _first = io.receive().await.expect("should receive first message");
    // This second receive blocks forever because there is no second message.
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

/// Builds a test server with persistence + sessions.
async fn test_server(store: Arc<InMemoryStore>) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store).without_auto_checkpoint());
    server.finish().await;

    let sessions = server.api::<SessionsAPI>().unwrap();
    let persistence = server.api::<PersistenceAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());

    sessions.register_agent(EchoAgent).unwrap();
    sessions.register_agent(BlockingAgent).unwrap();

    server
}

fn create_session(server: &Server, id: &SessionId, agent: &'static str) {
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .create_session(server.create_context(), id, &AgentTypeId::from_name(agent))
        .unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Happy path: send a message, receive the echoed response.
#[tokio::test]
async fn turn_echo_round_trip() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let id = SessionId::new();
    create_session(&server, &id, "EchoAgent");

    let (provider, input_tx, mut output_rx) = HttpIOProvider::new(32);
    let provider = Arc::new(provider);

    input_tx.send(IOMessage::user_text("hello")).await.unwrap();
    drop(input_tx);

    let io_provider = Arc::clone(&provider);
    let result = sessions
        .try_process_turn_with(&id, move |ctx| {
            ctx.insert(UserIO::new(io_provider));
        })
        .await
        .unwrap();

    assert!(result.nodes_executed > 0);

    let mut messages = Vec::new();
    while let Ok(msg) = output_rx.try_recv() {
        messages.push(msg);
    }

    assert_eq!(messages.len(), 1);
    assert!(matches!(
        messages[0].content,
        IOContent::Text(ref text) if text == "echo: hello"
    ));

    // Turn number should have incremented.
    let info = sessions.session_info(&id).unwrap();
    assert_eq!(info.turn_number, 1);
}

/// Unknown session returns `SessionNotFound`.
#[tokio::test]
async fn turn_unknown_session_returns_not_found() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();

    let bogus_id = SessionId::from_string("does-not-exist".to_owned());
    let result = sessions.try_process_turn(&bogus_id).await;

    assert!(matches!(result, Err(SessionError::SessionNotFound(_))));
}

/// Concurrent turn on the same session returns `SessionBusy`.
#[tokio::test]
async fn turn_concurrent_returns_busy() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let id = SessionId::new();
    create_session(&server, &id, "BlockingAgent");

    let (provider, input_tx, _output_rx) = HttpIOProvider::new(32);
    let provider = Arc::new(provider);

    input_tx.send(IOMessage::user_text("first")).await.unwrap();
    // Do NOT drop input_tx — keep the channel alive so the second
    // receive() in BlockingAgent blocks waiting for a message.

    let io_provider = Arc::clone(&provider);
    let sessions_clone = sessions.clone();
    let id_clone = id.clone();

    // Spawn the first turn — it will block on the second receive().
    let handle = tokio::spawn(async move {
        sessions_clone
            .try_process_turn_with(&id_clone, move |ctx| {
                ctx.insert(UserIO::new(io_provider));
            })
            .await
    });

    // Wait briefly for the first turn to acquire the lock.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The second turn should fail immediately with SessionBusy.
    let result = sessions.try_process_turn(&id).await;
    assert!(
        matches!(result, Err(SessionError::SessionBusy(_))),
        "expected SessionBusy, got: {result:?}"
    );

    // Clean up: drop input_tx to unblock the first turn, then
    // abort since the agent will return IOError::Closed.
    drop(input_tx);
    handle.abort();
}
