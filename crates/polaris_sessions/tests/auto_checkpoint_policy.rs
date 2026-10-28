//! Automatic checkpoints at each session boundary, and the two policies that
//! suppress them.
//!
//! One scripted lifecycle — create, a completed turn, an interrupted turn,
//! save, resume on a fresh server — runs under three policies:
//!
//! - the default, which captures at every boundary, so the script proves each
//!   boundary is one that *can* capture;
//! - auto-checkpoint off, which captures at none of them;
//! - a retention of `Some(0)` checkpoints, which refuses every capture. A
//!   refused automatic capture is logged, never propagated, so every step of
//!   the lifecycle still succeeds.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, Storable};
use polaris_graph::cancel::RunCancellation;
use polaris_graph::graph::Graph;
use polaris_sessions::checkpoint::CheckpointRetention;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId, SessionStore};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::{Res, ResMut};
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A persisted resource, so every capture carries a real payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Storable)]
#[storable(key = "Counter")]
struct Counter {
    value: u32,
}
impl LocalResource for Counter {}

/// Controls one turn of the script: while `park` is set, the agent's middle node
/// parks on the cancellation signal and tells the test it is in flight.
#[derive(Clone, Default)]
struct TurnGate {
    park: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
}
impl LocalResource for TurnGate {}

#[system]
async fn count(mut counter: ResMut<Counter>) {
    counter.value += 1;
}

#[system]
async fn park_when_asked(gate: Res<TurnGate>, cancel: Res<RunCancellation>) {
    if gate.park.load(Ordering::SeqCst) {
        gate.entered.notify_one();
        cancel.cancelled().await;
    }
}

/// Runs a node after the parking one, so an interrupt has a boundary to stop
/// at: a graph that ends on the parking node finishes successfully the moment
/// it returns.
#[system]
async fn after_park(_gate: Res<TurnGate>) {}

struct PolicyAgent;

impl Agent for PolicyAgent {
    fn build(&self, graph: &mut Graph) {
        graph
            .add_system(count)
            .add_system(park_when_asked)
            .add_system(after_park);
    }

    fn name(&self) -> &'static str {
        "PolicyAgent"
    }
}

/// How the script's server is configured.
#[derive(Clone, Copy)]
struct Policy {
    auto_checkpoint: bool,
    retention: CheckpointRetention,
}

async fn server_with(store: Arc<dyn SessionStore>, policy: Policy) -> Server {
    let mut server = Server::new();
    let mut plugin = SessionsPlugin::new(store).with_checkpoint_retention(policy.retention);
    if !policy.auto_checkpoint {
        plugin = plugin.without_auto_checkpoint();
    }
    server.add_plugins(PersistencePlugin).add_plugins(plugin);
    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    persistence.register::<Counter>("test");
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(PolicyAgent).unwrap();
    server
}

/// Retained checkpoints after each step of the script.
#[derive(Debug, PartialEq, Eq)]
struct Counts {
    after_create: usize,
    after_completed_turn: usize,
    after_interrupted_turn: usize,
    after_resume: usize,
}

fn retained(sessions: &SessionsAPI, id: &SessionId) -> usize {
    sessions.checkpoints().list(id).unwrap().len()
}

/// Runs the lifecycle under `policy`, asserting that every step succeeds, and
/// reports how many checkpoints each step left behind.
async fn run_lifecycle(policy: Policy) -> Counts {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    let id = SessionId::new();
    let gate = TurnGate::default();

    let server = server_with(Arc::clone(&store), policy).await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions
        .create_session_with(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("PolicyAgent"),
            {
                let gate = gate.clone();
                move |ctx| {
                    ctx.insert(Counter::default());
                    ctx.insert(gate);
                }
            },
        )
        .expect("creating a session never depends on the checkpoint policy");
    let after_create = retained(sessions, &id);

    sessions
        .process_turn(&id)
        .await
        .expect("a completed turn never depends on the checkpoint policy");
    let after_completed_turn = retained(sessions, &id);

    gate.park.store(true, Ordering::SeqCst);
    let turn = tokio::spawn({
        let sessions = sessions.clone();
        let id = id.clone();
        async move { sessions.process_turn(&id).await }
    });
    gate.entered.notified().await;
    assert!(
        sessions.interrupt_turn(&id).unwrap(),
        "the second turn was in flight"
    );
    match turn.await.unwrap() {
        Err(SessionError::Interrupted { .. }) => {}
        other => panic!("expected SessionError::Interrupted, got {other:?}"),
    }
    let after_interrupted_turn = retained(sessions, &id);

    sessions.save_session(&id).await.unwrap();

    // Resume on a fresh server over the same store, as a restarted host would.
    gate.park.store(false, Ordering::SeqCst);
    let resumed = server_with(store, policy).await;
    let sessions = resumed.api::<SessionsAPI>().unwrap();
    sessions
        .resume_session_with(resumed.create_context(), &id, move |ctx| {
            ctx.insert(gate);
        })
        .await
        .expect("resuming a session never depends on the checkpoint policy");
    let after_resume = retained(sessions, &id);

    assert_eq!(
        sessions.session_info(&id).unwrap().turn_number,
        2,
        "both turns advanced the number, whatever the checkpoint policy"
    );
    sessions
        .process_turn(&id)
        .await
        .expect("the resumed session runs a turn");

    Counts {
        after_create,
        after_completed_turn,
        after_interrupted_turn,
        after_resume,
    }
}

/// The control for the two suppression tests: under the default policy each
/// step adds exactly one point, so a zero in the other two tests is the policy
/// at work rather than a boundary that never captured.
#[tokio::test]
async fn the_default_policy_captures_at_every_boundary() {
    let counts = run_lifecycle(Policy {
        auto_checkpoint: true,
        retention: CheckpointRetention::default(),
    })
    .await;

    assert_eq!(
        counts,
        Counts {
            after_create: 1,
            after_completed_turn: 2,
            after_interrupted_turn: 3,
            after_resume: 4,
        }
    );
}

#[tokio::test]
async fn with_auto_checkpoint_off_no_boundary_captures() {
    let counts = run_lifecycle(Policy {
        auto_checkpoint: false,
        retention: CheckpointRetention::default(),
    })
    .await;

    assert_eq!(
        counts,
        Counts {
            after_create: 0,
            after_completed_turn: 0,
            after_interrupted_turn: 0,
            after_resume: 0,
        },
        "no automatic checkpoint at create, after a turn, after an interrupt, or at resume"
    );
}

#[tokio::test]
async fn a_zero_count_retention_refuses_every_capture_without_failing_the_lifecycle() {
    let counts = run_lifecycle(Policy {
        auto_checkpoint: true,
        retention: CheckpointRetention::new(Some(0), None),
    })
    .await;

    assert_eq!(
        counts,
        Counts {
            after_create: 0,
            after_completed_turn: 0,
            after_interrupted_turn: 0,
            after_resume: 0,
        },
        "a refused automatic capture is logged, and nothing is retained"
    );
}
