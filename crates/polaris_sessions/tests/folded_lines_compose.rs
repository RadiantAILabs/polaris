//! Interrupts and turn history compose without the HTTP feature.
//!
//! An interrupted turn is a *recorded* outcome on a host that never enables
//! `sessions-http`: the history, the checkpoint and the message recording that
//! describe it are all reachable through `SessionsAPI` alone.
//!
//! The history and recording methods (`turn_history`, `turn`,
//! `record_turn_messages`) are not gated behind `sessions-http`, and this
//! target is not gated either. If a gate came back, the target would stop
//! compiling only in a run that leaves `sessions-http` off, which is the CI
//! step `cargo test -p polaris_sessions --tests` with default features. A run
//! with `--all-features` enables the feature and would stay green.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_core_plugins::{IOContent, IOMessage};
use polaris_graph::cancel::RunCancellation;
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin, TurnStatus};
use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Lets the test observe that a turn genuinely entered its first node, so no
/// assertion depends on wall-clock timing.
#[derive(Clone, Default)]
struct TurnGate {
    entered: Arc<tokio::sync::Notify>,
    bodies: Arc<AtomicU32>,
    /// Set if the node after the parking one ever starts.
    second_node_ran: Arc<AtomicBool>,
}

impl LocalResource for TurnGate {}

/// Parks until the run is interrupted, then returns normally.
#[system]
async fn park_until_interrupted(gate: Res<TurnGate>, cancel: Res<RunCancellation>) {
    gate.bodies.fetch_add(1, Ordering::SeqCst);
    gate.entered.notify_one();
    cancel.cancelled().await;
}

#[system]
async fn second_node(gate: Res<TurnGate>) {
    gate.second_node_ran.store(true, Ordering::SeqCst);
}

/// Two nodes: the first parks on the cancellation token, the second must never
/// start once the token is raised. A single-node graph would finish
/// *successfully* the moment the parked body returned, because the executor
/// only observes the raise at a node boundary.
struct InterruptibleAgent;

impl Agent for InterruptibleAgent {
    fn build(&self, graph: &mut Graph) {
        graph
            .add_system(park_until_interrupted)
            .add_system(second_node);
    }

    fn name(&self) -> &'static str {
        "ComposeInterruptible"
    }
}

async fn test_server() -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(InterruptibleAgent).unwrap();
    server
}

fn create_session(server: &Server, id: &SessionId, gate: &TurnGate) {
    let sessions = server.api::<SessionsAPI>().unwrap();
    let gate = gate.clone();
    sessions
        .create_session_with(
            server.create_context(),
            id,
            &AgentTypeId::from_name("ComposeInterruptible"),
            move |ctx| {
                ctx.insert(gate);
            },
        )
        .unwrap();
}

/// Runs one turn and interrupts it once it is genuinely in flight, returning
/// the interrupted turn number.
async fn interrupt_one_turn(server: &Server, id: &SessionId, gate: &TurnGate) -> u32 {
    let sessions = server.api::<SessionsAPI>().unwrap();
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });

    gate.entered.notified().await;
    assert!(
        sessions.interrupt_turn(id).expect("the session exists"),
        "a turn was in flight"
    );

    match turn.await.unwrap() {
        Err(SessionError::Interrupted { turn, .. }) => turn,
        other => panic!("expected SessionError::Interrupted, got {other:?}"),
    }
}

/// The interrupt line produces `TurnStatus::Interrupted`; the ungated-history
/// line is what makes it readable on a host with no `sessions-http`.
#[tokio::test]
async fn an_interrupted_turn_is_recorded_as_interrupted_without_the_http_feature() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, &gate);

    let interrupted_turn = interrupt_one_turn(&server, &id, &gate).await;

    let summaries = sessions
        .turn_history(&id, false)
        .expect("history is readable without the sessions-http feature");
    assert_eq!(summaries.len(), 1, "the interrupted turn is one record");
    assert_eq!(
        summaries[0].status,
        TurnStatus::Interrupted,
        "an interrupt is its own recorded outcome, never Failed and never \
         left Running"
    );
    assert_eq!(summaries[0].turn, interrupted_turn);
    assert!(
        summaries[0].finished_at.is_some(),
        "the record is finalized, so a reader never sees an open turn"
    );
    assert_eq!(gate.bodies.load(Ordering::SeqCst), 1);
    assert!(
        !gate.second_node_ran.load(Ordering::SeqCst),
        "the turn stopped at the node boundary, so nothing ran after it"
    );
}

/// The checkpoint the interrupt path captures and the history record the
/// ungated line writes describe the same turn.
#[tokio::test]
async fn the_interrupt_checkpoint_and_the_ungated_history_name_the_same_turn() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, &gate);

    let checkpoints = sessions.checkpoints();
    let before = checkpoints.list(&id).expect("the session exists").len();

    let interrupted_turn = interrupt_one_turn(&server, &id, &gate).await;

    let after = checkpoints.list(&id).expect("the session exists");
    assert_eq!(
        after.len(),
        before + 1,
        "an interrupted turn is a legitimate rollback target, so it captures"
    );
    let latest = after.last().expect("at least one checkpoint");
    assert_eq!(
        latest.turn, interrupted_turn,
        "the checkpoint's display label is the turn the history recorded"
    );
    assert_eq!(
        latest.resume_turn,
        interrupted_turn + 1,
        "the turn number advanced past the interrupted turn"
    );

    let summaries = sessions.turn_history(&id, false).unwrap();
    assert_eq!(summaries[0].turn, latest.turn);
    assert_eq!(summaries[0].status, TurnStatus::Interrupted);
}

/// An interrupted turn is a first-class history entry: the ungated recording
/// path attaches messages to it and reads them back.
#[tokio::test]
async fn messages_can_be_recorded_against_an_interrupted_turn_without_http() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, &gate);

    let interrupted_turn = interrupt_one_turn(&server, &id, &gate).await;

    sessions.record_turn_messages(
        &id,
        interrupted_turn,
        vec![IOMessage::user_text("partial output before the interrupt")],
    );

    let record = sessions
        .turn(&id, interrupted_turn)
        .expect("the interrupted turn is addressable by number");
    assert_eq!(record.status, TurnStatus::Interrupted);
    assert_eq!(record.messages.len(), 1);
    assert!(matches!(&record.messages[0].content, IOContent::Text(text)
            if text == "partial output before the interrupt"));
}
