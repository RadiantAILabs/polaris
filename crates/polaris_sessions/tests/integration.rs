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
use polaris_system::param::{Res, ResMut};
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

#[derive(Debug, Clone, PartialEq)]
struct DoubleOutput {
    result: u64,
}

#[derive(Debug, Clone)]
struct DoubleInput {
    value: u64,
}
impl LocalResource for DoubleInput {}

#[system]
async fn double(input: Res<DoubleInput>) -> DoubleOutput {
    DoubleOutput {
        result: input.value * 2,
    }
}

struct DoubleAgent;

impl Agent for DoubleAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(double);
    }

    fn name(&self) -> &'static str {
        "DoubleAgent"
    }
}

/// Signal used to coordinate a turn that blocks mid-execution.
///
/// `started` is fired by the system once it begins running (with the ctx
/// lock held). `proceed` gates the system's completion until the test
/// allows it.
#[derive(Debug, Clone)]
struct BlockingSignal {
    started: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
}
impl LocalResource for BlockingSignal {}

#[system]
async fn blocking_step(signal: Res<BlockingSignal>) {
    signal.started.notify_one();
    signal.proceed.notified().await;
}

struct BlockingAgent;

impl Agent for BlockingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(blocking_step);
    }

    fn name(&self) -> &'static str {
        "BlockingAgent"
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

// ─────────────────────────────────────────────────────────────────────────────
// One-shot execution tests
// ─────────────────────────────────────────────────────────────────────────────

/// `run_oneshot` returns the terminal system's output and cleans up the session.
#[tokio::test]
async fn run_oneshot_returns_output() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(DoubleAgent).unwrap();
    let agent_type = AgentTypeId::from_name("DoubleAgent");

    let output: DoubleOutput = sessions
        .run_oneshot(&agent_type, |ctx| {
            ctx.insert(DoubleInput { value: 21 });
        })
        .await
        .unwrap();

    assert_eq!(output, DoubleOutput { result: 42 });
    assert!(sessions.list_live_sessions().is_empty());
}

/// `run_oneshot` returns `OutputNotFound` when the graph produces a different type.
#[tokio::test]
async fn run_oneshot_output_not_found() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let result = sessions
        .run_oneshot::<DoubleOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await;

    assert!(matches!(result, Err(SessionError::OutputNotFound(_))));
    assert!(sessions.list_live_sessions().is_empty());
}

/// `run_oneshot` returns `AgentNotFound` for unregistered agents.
#[tokio::test]
async fn run_oneshot_agent_not_found() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let unknown = AgentTypeId::from_name("NonexistentAgent");

    let result = sessions.run_oneshot::<DoubleOutput>(&unknown, |_| {}).await;

    assert!(matches!(result, Err(SessionError::AgentNotFound(_))));
}

// ─────────────────────────────────────────────────────────────────────────────
// Scoped session (RAII guard) tests
// ─────────────────────────────────────────────────────────────────────────────

/// `scoped_session` creates a guard that can process turns.
#[tokio::test]
async fn scoped_session_processes_turns() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let guard = sessions
        .scoped_session(&AgentTypeId::from_name("CounterAgent"), |ctx| {
            ctx.insert(Counter::default());
        })
        .unwrap();

    guard.process_turn().await.unwrap();
    let value: u32 = guard
        .with_context(|ctx| ctx.get_resource::<Counter>().unwrap().value)
        .await
        .unwrap();
    assert_eq!(value, 1);
}

/// The guard deletes the session on drop.
#[tokio::test]
async fn scoped_session_cleans_up_on_drop() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let id = {
        let guard = sessions
            .scoped_session(&AgentTypeId::from_name("CounterAgent"), |ctx| {
                ctx.insert(Counter::default());
            })
            .unwrap();
        guard.process_turn().await.unwrap();
        guard.id().clone()
    };

    // Give the spawned cleanup task time to run.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(!sessions.list_live_sessions().contains(&id));
}

/// `scoped_session` returns `AgentNotFound` for unregistered agents.
#[tokio::test]
async fn scoped_session_agent_not_found() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let unknown = AgentTypeId::from_name("NonexistentAgent");

    let result = sessions.scoped_session(&unknown, |_| {});
    assert!(matches!(result, Err(SessionError::AgentNotFound(_))));
}

/// `run_oneshot` cleans up the session even when graph execution fails.
///
/// Guards against regressions in the guaranteed-cleanup contract: the
/// method promises cleanup in all exit paths, not just success and
/// `OutputNotFound`.
#[tokio::test]
async fn run_oneshot_cleans_up_on_execution_error() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(DoubleAgent).unwrap();
    let agent_type = AgentTypeId::from_name("DoubleAgent");

    // Intentionally omit `DoubleInput` — `Res<DoubleInput>` will fail to
    // resolve and surface as an `ExecutionError` during `process_turn`.
    let result = sessions
        .run_oneshot::<DoubleOutput>(&agent_type, |_| {})
        .await;

    assert!(
        matches!(result, Err(SessionError::Execution(_))),
        "expected Execution error, got {result:?}"
    );
    assert!(
        sessions.list_live_sessions().is_empty(),
        "session must be cleaned up after execution error"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// try_process_turn tests
// ─────────────────────────────────────────────────────────────────────────────

/// `try_process_turn` returns `SessionBusy` when another turn holds the ctx lock.
#[tokio::test]
async fn try_process_turn_returns_session_busy_while_turn_in_flight() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(BlockingAgent).unwrap();
    let agent_type = AgentTypeId::from_name("BlockingAgent");

    let signal = BlockingSignal {
        started: Arc::new(tokio::sync::Notify::new()),
        proceed: Arc::new(tokio::sync::Notify::new()),
    };

    let id = SessionId::new();
    sessions
        .create_session_with(server.create_context(), &id, &agent_type, {
            let signal = signal.clone();
            move |ctx| {
                ctx.insert(signal);
            }
        })
        .unwrap();

    // Spawn the first turn; it will acquire the ctx lock and block inside
    // `blocking_step` until we signal `proceed`.
    let sessions_bg = sessions.clone();
    let id_bg = id.clone();
    let handle = tokio::spawn(async move { sessions_bg.process_turn(&id_bg).await });

    // Wait for the spawned turn to enter the blocking system — this
    // guarantees the ctx lock is held when we call `try_process_turn`.
    signal.started.notified().await;

    let result = sessions.try_process_turn(&id).await;
    assert!(
        matches!(&result, Err(SessionError::SessionBusy(busy_id)) if busy_id == &id),
        "expected SessionBusy({id:?}), got {result:?}"
    );

    // Release the in-flight turn and ensure it completes cleanly.
    signal.proceed.notify_one();
    handle
        .await
        .expect("spawned task did not panic")
        .expect("background turn should succeed");
}

/// `try_process_turn` returns `SessionNotFound` for an unknown session.
#[tokio::test]
async fn try_process_turn_returns_session_not_found() {
    let store = Arc::new(InMemoryStore::new());
    let server = test_server(Arc::clone(&store)).await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let missing = SessionId::new();
    let result = sessions.try_process_turn(&missing).await;
    assert!(
        matches!(&result, Err(SessionError::SessionNotFound(id)) if id == &missing),
        "expected SessionNotFound, got {result:?}"
    );
}
