//! Integration tests for checkpoint, rollback, and persistence HTTP endpoints.
//!
//! Tests exercise the [`SessionsAPI`] methods that back the HTTP handlers:
//! `checkpoint`, `list_checkpoints`, `rollback`, `save_session`,
//! `resume_session`, and `list_sessions` (store).

#![cfg(feature = "http")]

use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, Storable};
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId, SessionStore};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::ResMut;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, Storable)]
#[storable(key = "Counter")]
struct Counter {
    value: u32,
}
impl LocalResource for Counter {}

#[system]
async fn increment(mut counter: ResMut<Counter>) {
    counter.value += 1;
}

struct CounterAgent;

impl Agent for CounterAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(increment);
    }

    fn name(&self) -> &'static str {
        "CounterAgent"
    }
}

async fn test_server(store: Arc<InMemoryStore>) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store).without_auto_checkpoint());
    server.finish().await;

    let persistence = server.api::<PersistenceAPI>().unwrap();
    persistence.register::<Counter>("test");

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CounterAgent).unwrap();

    server
}

fn create_session(server: &Server, id: &SessionId) {
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .create_session_with(
            server.create_context(),
            id,
            &AgentTypeId::from_name("CounterAgent"),
            |ctx| {
                ctx.insert(Counter::default());
            },
        )
        .unwrap();
}

async fn read_counter(store: &InMemoryStore, id: &SessionId) -> u32 {
    let data = store.load(id).await.unwrap().expect("session should exist");
    let entry = data
        .resources
        .iter()
        .find(|r| r.storage_key == "Counter")
        .expect("Counter should be persisted");
    let counter: Counter = serde_json::from_value(entry.data.clone()).unwrap();
    counter.value
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoint / rollback tests
// ─────────────────────────────────────────────────────────────────────────────

/// Create checkpoint, list it, rollback, verify state restored.
#[tokio::test]
async fn checkpoint_list_and_rollback() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    // 3 turns → counter = 3.
    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }

    // Checkpoint at turn 3.
    let cp = sessions.checkpoint(&id).await.unwrap();
    assert_eq!(cp, 3);

    // 2 more turns → counter = 5.
    for _ in 0..2 {
        sessions.process_turn(&id).await.unwrap();
    }

    // List checkpoints — should contain exactly turn 3.
    let list = sessions.list_checkpoints(&id).unwrap();
    assert_eq!(list, vec![3]);

    // Rollback to turn 3 → counter should be 3 again.
    sessions.rollback(&id, 3).await.unwrap();

    let info = sessions.session_info(&id).unwrap();
    assert_eq!(info.turn_number, 3);

    // Verify counter value via persistence.
    sessions.save_session(&id).await.unwrap();
    assert_eq!(read_counter(&store, &id).await, 3);
}

/// Multiple checkpoints at different turns.
#[tokio::test]
async fn multiple_checkpoints() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    sessions.process_turn(&id).await.unwrap();
    sessions.checkpoint(&id).await.unwrap(); // turn 1

    sessions.process_turn(&id).await.unwrap();
    sessions.checkpoint(&id).await.unwrap(); // turn 2

    sessions.process_turn(&id).await.unwrap();
    sessions.checkpoint(&id).await.unwrap(); // turn 3

    let list = sessions.list_checkpoints(&id).unwrap();
    assert_eq!(list, vec![1, 2, 3]);

    // Rollback to turn 1 discards checkpoints for 2 and 3.
    sessions.rollback(&id, 1).await.unwrap();

    let list = sessions.list_checkpoints(&id).unwrap();
    assert_eq!(list, vec![1]);
}

/// Unknown session returns `SessionNotFound`.
#[tokio::test]
async fn checkpoint_unknown_session() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let bogus = SessionId::from_string("does-not-exist".to_owned());

    assert!(matches!(
        sessions.checkpoint(&bogus).await,
        Err(SessionError::SessionNotFound(_))
    ));
    assert!(matches!(
        sessions.list_checkpoints(&bogus),
        Err(SessionError::SessionNotFound(_))
    ));
    assert!(matches!(
        sessions.rollback(&bogus, 0).await,
        Err(SessionError::SessionNotFound(_))
    ));
}

/// Rollback to non-existent turn returns `TurnNotFound`.
#[tokio::test]
async fn rollback_invalid_turn() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    create_session(&server, &id);

    sessions.process_turn(&id).await.unwrap();

    let result = sessions.rollback(&id, 99).await;
    assert!(matches!(result, Err(SessionError::TurnNotFound(99))));
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistence (store) tests
// ─────────────────────────────────────────────────────────────────────────────

/// Save session, resume on fresh server, verify state.
#[tokio::test]
async fn save_and_resume_via_api() {
    let store = Arc::new(InMemoryStore::new());
    let id = SessionId::new();

    // First server: create, run 2 turns, save.
    {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_session(&server, &id);

        sessions.process_turn(&id).await.unwrap();
        sessions.process_turn(&id).await.unwrap();
        sessions.save_session(&id).await.unwrap();
    }

    // Second server: resume, verify counter = 2, run 1 more turn.
    {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();

        sessions
            .resume_session(sessions.create_context(), &id)
            .await
            .unwrap();

        let info = sessions.session_info(&id).unwrap();
        assert_eq!(info.turn_number, 2);

        sessions.process_turn(&id).await.unwrap();
        sessions.save_session(&id).await.unwrap();
        assert_eq!(read_counter(&store, &id).await, 3);
    }
}

/// List stored sessions returns IDs from the backing store.
#[tokio::test]
async fn list_stored_sessions() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    // Initially empty.
    let stored = sessions.list_sessions().await.unwrap();
    assert!(stored.is_empty());

    // Create and save two sessions.
    let id_a = SessionId::from_string("session-a".to_owned());
    let id_b = SessionId::from_string("session-b".to_owned());
    create_session(&server, &id_a);
    create_session(&server, &id_b);
    sessions.save_session(&id_a).await.unwrap();
    sessions.save_session(&id_b).await.unwrap();

    let mut stored: Vec<String> = sessions
        .list_sessions()
        .await
        .unwrap()
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    stored.sort();
    assert_eq!(stored, vec!["session-a", "session-b"]);
}

/// Resume unknown session returns `SessionNotFound`.
#[tokio::test]
async fn resume_unknown_session() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let bogus = SessionId::from_string("nonexistent".to_owned());

    let result = sessions
        .resume_session(sessions.create_context(), &bogus)
        .await;
    assert!(matches!(result, Err(SessionError::SessionNotFound(_))));
}
