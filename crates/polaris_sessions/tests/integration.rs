//! Integration tests for [`SessionsAPI`].
//!
//! Verifies session lifecycle: multi-turn execution, checkpoint/rollback,
//! save/resume, and session isolation.

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

/// Builds a server with persistence + sessions (auto-checkpoint disabled).
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

fn create_test_session(server: &Server, id: &SessionId) {
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

/// Reads the persisted counter value from the store.
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
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Resources survive across multiple turns and are correctly persisted.
#[tokio::test]
async fn multi_turn_and_save() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();

    create_test_session(&server, &id);

    for _ in 0..3 {
        sessions.process_turn(&id).await.unwrap();
    }

    sessions.save_session(&id).await.unwrap();
    assert_eq!(read_counter(&store, &id).await, 3);
}

/// Checkpoint captures state; rollback restores it.
#[tokio::test]
async fn checkpoint_and_rollback() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();

    create_test_session(&server, &id);

    // Turn 0 → counter = 1.
    sessions.process_turn(&id).await.unwrap();
    let cp_turn = sessions.checkpoint(&id).await.unwrap();
    assert_eq!(cp_turn, 1);

    // Two more turns → counter = 3.
    for turn in 0..2 {
        sessions.process_turn(&id).await.unwrap();

        // Verify counter value after each turn (should be 2, then 3).
        sessions.save_session(&id).await.unwrap();
        let expected = if turn == 0 { 2 } else { 3 };
        assert_eq!(read_counter(&store, &id).await, expected);
    }

    // Rollback to checkpoint (counter = 1).
    sessions.rollback(&id, cp_turn).await.unwrap();

    sessions.save_session(&id).await.unwrap();
    assert_eq!(read_counter(&store, &id).await, 1);
}

/// Save, simulate restart with a fresh server, resume, then continue.
#[tokio::test]
async fn save_and_resume() {
    let store = Arc::new(InMemoryStore::new());
    let id = SessionId::new();

    // First "process lifetime": run 2 turns and save.
    {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();
        create_test_session(&server, &id);

        for _ in 0..2 {
            sessions.process_turn(&id).await.unwrap();
        }

        // Verify counter value is 2 before saving.
        sessions.save_session(&id).await.unwrap();
        assert_eq!(read_counter(&store, &id).await, 2);
    }

    // Second "process lifetime": fresh server, same store.
    {
        let server = test_server(Arc::clone(&store)).await;
        let sessions = server.api::<SessionsAPI>().unwrap();

        sessions
            .resume_session(server.create_context(), &id)
            .await
            .unwrap();
        sessions.process_turn(&id).await.unwrap();

        sessions.save_session(&id).await.unwrap();
        assert_eq!(read_counter(&store, &id).await, 3);
    }
}

/// Two sessions on the same server don't share state.
#[tokio::test]
async fn session_isolation() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let id_a = SessionId::new();
    let id_b = SessionId::new();
    create_test_session(&server, &id_a);
    create_test_session(&server, &id_b);

    for _ in 0..3 {
        sessions.process_turn(&id_a).await.unwrap();
    }
    sessions.process_turn(&id_b).await.unwrap();

    sessions.save_session(&id_a).await.unwrap();
    sessions.save_session(&id_b).await.unwrap();

    assert_eq!(read_counter(&store, &id_a).await, 3);
    assert_eq!(read_counter(&store, &id_b).await, 1);
}

/// Creating a session with an unregistered agent returns an error.
#[tokio::test]
async fn unregistered_agent_errors() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let err = sessions
        .create_session(
            server.create_context(),
            &SessionId::new(),
            &AgentTypeId::from_name("UnknownAgent"),
        )
        .unwrap_err();
    assert!(matches!(err, SessionError::AgentNotFound(_)));
}
