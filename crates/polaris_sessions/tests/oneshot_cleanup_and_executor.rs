//! One-shot and scoped sessions: cleanup when the caller's future is dropped,
//! and per-call executors.
//!
//! A graph that runs agent calls as parallel branches drops a sibling's
//! future when one branch fails. These tests pin that such a dropped call
//! leaves no live session behind, and that a call can cap its own turn with a
//! custom [`GraphExecutor`].

use polaris_agent::Agent;
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_graph::graph::Graph;
use polaris_graph::{ExecutionError, GraphExecutor};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionData, SessionId, SessionStore};
use polaris_sessions::{SessionError, SessionGuard, SessionsAPI, SessionsPlugin};
use polaris_system::param::{Res, ResMut};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use polaris_system::system::BoxFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// `started` fires once the turn is running; the turn then waits forever, so
/// it only ends when its future is dropped.
#[derive(Debug, Clone, Default)]
struct Started(Arc<tokio::sync::Notify>);
impl LocalResource for Started {}

#[system]
async fn wait_forever(started: Res<Started>) -> u32 {
    started.0.notify_one();
    std::future::pending::<()>().await;
    0
}

struct StuckAgent;

impl Agent for StuckAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(wait_forever);
    }

    fn name(&self) -> &'static str {
        "StuckAgent"
    }
}

#[derive(Debug, Clone, Default)]
struct Spins(u32);
impl LocalResource for Spins {}

#[derive(Debug, Clone)]
struct SpinState;

#[system]
async fn start_spinning() -> SpinState {
    SpinState
}

#[system]
async fn spin(mut spins: ResMut<Spins>) -> SpinState {
    spins.0 += 1;
    SpinState
}

/// A loop whose predicate never ends it, so only an iteration cap stops it.
struct SpinAgent;

impl Agent for SpinAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(start_spinning);
        graph.add_loop::<SpinState, _, _>(
            "spin_forever",
            |_| false,
            |g| {
                g.add_system(spin);
            },
        );
    }

    fn name(&self) -> &'static str {
        "SpinAgent"
    }
}

/// An [`InMemoryStore`] that counts `delete` calls. With `stall_first`, the
/// first `delete` never finishes.
#[derive(Default)]
struct CountingStore {
    inner: InMemoryStore,
    deletes: AtomicUsize,
    stall_first: bool,
}

impl SessionStore for CountingStore {
    fn save(&self, id: &SessionId, data: &SessionData) -> BoxFuture<'_, Result<(), SessionError>> {
        self.inner.save(id, data)
    }

    fn load(&self, id: &SessionId) -> BoxFuture<'_, Result<Option<SessionData>, SessionError>> {
        self.inner.load(id)
    }

    fn delete(&self, id: &SessionId) -> BoxFuture<'_, Result<(), SessionError>> {
        let n = self.deletes.fetch_add(1, Ordering::SeqCst);
        if self.stall_first && n == 0 {
            return Box::pin(std::future::pending());
        }
        self.inner.delete(id)
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<SessionId>, SessionError>> {
        self.inner.list()
    }
}

async fn test_server() -> Server {
    test_server_with(Arc::new(InMemoryStore::new())).await
}

async fn test_server_with(store: Arc<dyn SessionStore>) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store).without_auto_checkpoint());
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(StuckAgent).unwrap();
    sessions.register_agent(SpinAgent).unwrap();
    server
}

fn stuck() -> AgentTypeId {
    AgentTypeId::from_name("StuckAgent")
}

fn spinner() -> AgentTypeId {
    AgentTypeId::from_name("SpinAgent")
}

/// Starts `call` on its own task, waits until its turn is running, checks that
/// exactly one session is live, then aborts the task and waits for the drop.
async fn abort_mid_turn<F>(sessions: &SessionsAPI, started: &Started, call: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = tokio::spawn(call);
    tokio::time::timeout(Duration::from_secs(5), started.0.notified())
        .await
        .expect("the turn started");
    assert_eq!(
        sessions.list_live_sessions().len(),
        1,
        "the call's session is live while its turn runs"
    );

    task.abort();
    let join = task.await;
    assert!(
        join.is_err_and(|err| err.is_cancelled()),
        "the task was cancelled, not completed"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Dropped futures leave no live session
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_oneshot_dropped_mid_turn_removes_its_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let started = Started::default();

    let call_sessions = sessions.clone();
    let call_started = started.clone();
    abort_mid_turn(sessions, &started, async move {
        call_sessions
            .run_oneshot::<u32>(&stuck(), move |ctx| {
                ctx.insert(call_started);
            })
            .await
    })
    .await;

    assert!(
        sessions.list_live_sessions().is_empty(),
        "dropping the one-shot future removes its session"
    );
}

#[tokio::test]
async fn run_oneshot_with_executor_dropped_mid_turn_removes_its_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let started = Started::default();

    let call_sessions = sessions.clone();
    let call_started = started.clone();
    abort_mid_turn(sessions, &started, async move {
        call_sessions
            .run_oneshot_with_executor::<u32>(&stuck(), GraphExecutor::new(), move |ctx| {
                ctx.insert(call_started);
            })
            .await
    })
    .await;

    assert!(sessions.list_live_sessions().is_empty());
}

#[tokio::test]
async fn run_oneshot_preserved_dropped_mid_turn_removes_its_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let started = Started::default();

    let call_sessions = sessions.clone();
    let call_started = started.clone();
    abort_mid_turn(sessions, &started, async move {
        call_sessions
            .run_oneshot_preserved::<u32>(&stuck(), move |ctx| {
                ctx.insert(call_started);
            })
            .await
    })
    .await;

    assert!(
        sessions.list_live_sessions().is_empty(),
        "a preserved one-shot that never finished is not kept"
    );
}

/// The real case: two calls race as siblings, one fails, and the other's
/// future is dropped while its turn is still running.
#[tokio::test]
async fn failing_sibling_does_not_leave_the_other_call_open() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let started = Started::default();
    let stuck = stuck();
    let unregistered = AgentTypeId::from_name("Unregistered");

    let slow = sessions.run_oneshot::<u32>(&stuck, {
        let started = started.clone();
        move |ctx| {
            ctx.insert(started);
        }
    });
    let failing = async {
        started.0.notified().await;
        sessions.run_oneshot::<u32>(&unregistered, |_| {}).await
    };

    let result: Result<(u32, u32), SessionError> = tokio::try_join!(slow, failing);

    assert!(
        matches!(result, Err(SessionError::AgentNotFound(name)) if name.contains("Unregistered"))
    );
    assert!(
        sessions.list_live_sessions().is_empty(),
        "the dropped sibling's session is gone"
    );
}

#[test]
fn session_guard_dropped_outside_a_runtime_still_removes_its_session() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(test_server());
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let guard = sessions.scoped_session(&stuck(), |_| {}).unwrap();
    let id = guard.id().clone();
    assert!(sessions.list_live_sessions().contains(&id));

    // No runtime on this thread, so no task can run the store deletion.
    std::thread::spawn(move || drop(guard)).join().unwrap();

    assert!(
        !sessions.list_live_sessions().contains(&id),
        "the live entry is removed synchronously on drop"
    );
}

#[tokio::test]
async fn session_guard_delete_removes_the_session_and_disarms_drop() {
    let store = Arc::new(CountingStore::default());
    let server = test_server_with(store.clone()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let guard = sessions.scoped_session(&stuck(), |_| {}).unwrap();
    let id = guard.id().clone();

    guard.delete().await.unwrap();
    // Give a wrongly spawned `Drop` cleanup the chance to run.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        store.deletes.load(Ordering::SeqCst),
        1,
        "`Drop` does not delete a second time"
    );

    assert!(!sessions.list_live_sessions().contains(&id));
    assert!(matches!(
        sessions.session_info(&id),
        Err(SessionError::SessionNotFound(_))
    ));
}

/// A cancelled `delete` falls back to the `Drop` cleanup, which deletes the
/// stored copy on a spawned task.
#[tokio::test]
async fn cancelled_session_guard_delete_still_deletes_the_stored_copy() {
    let store = Arc::new(CountingStore {
        stall_first: true,
        ..CountingStore::default()
    });
    let server = test_server_with(store.clone()).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let guard = sessions.scoped_session(&stuck(), |_| {}).unwrap();
    sessions.save_session(guard.id()).await.unwrap();
    assert_eq!(sessions.list_sessions().await.unwrap().len(), 1);

    // The first store delete never finishes, so the timeout drops `delete`.
    let cancelled = tokio::time::timeout(Duration::from_millis(50), guard.delete()).await;
    assert!(cancelled.is_err(), "the stalled delete was cancelled");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !sessions.list_sessions().await.unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the `Drop` fallback did not delete the stored copy within 5s"
        );
        tokio::task::yield_now().await;
    }
}

/// A guard held as a resource of another session is dropped when that
/// session is deleted. Its `Drop` must not deadlock on the session map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_session_that_holds_a_guard_does_not_deadlock() {
    struct HeldGuard {
        _guard: SessionGuard,
    }
    impl LocalResource for HeldGuard {}

    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let inner = sessions.scoped_session(&stuck(), |_| {}).unwrap();
    let inner_id = inner.id().clone();
    let outer = sessions
        .scoped_session(&stuck(), move |ctx| {
            ctx.insert(HeldGuard { _guard: inner });
        })
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), outer.delete())
        .await
        .expect("deleting the outer session deadlocked")
        .unwrap();
    assert!(!sessions.list_live_sessions().contains(&inner_id));
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-call executors
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_oneshot_with_executor_applies_the_iteration_cap() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let result = sessions
        .run_oneshot_with_executor::<SpinState>(
            &spinner(),
            GraphExecutor::new().with_default_max_iterations(3),
            |ctx| {
                ctx.insert(Spins::default());
            },
        )
        .await;

    match result {
        Err(SessionError::Execution(ExecutionError::MaxIterationsExceeded { max, .. })) => {
            assert_eq!(max, 3, "the call's own cap applies");
        }
        other => panic!("expected MaxIterationsExceeded, got {other:?}"),
    }
    assert!(sessions.list_live_sessions().is_empty());
}

#[tokio::test]
async fn scoped_session_with_executor_caps_every_turn_and_exposes_the_context() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let guard = sessions
        .scoped_session_with_executor(
            &spinner(),
            GraphExecutor::new().with_default_max_iterations(4),
            |ctx| {
                ctx.insert(Spins::default());
            },
        )
        .unwrap();

    let err = guard.process_turn().await.unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::Execution(ExecutionError::MaxIterationsExceeded { max: 4, .. })
        ),
        "expected the cap of 4, got {err:?}"
    );

    let spins = guard
        .with_context(|ctx| ctx.get_resource::<Spins>().unwrap().0)
        .await
        .unwrap();
    assert_eq!(spins, 4, "the loop body ran exactly up to the cap");

    let id: SessionId = guard.id().clone();
    guard.delete().await.unwrap();
    assert!(!sessions.list_live_sessions().contains(&id));
}
