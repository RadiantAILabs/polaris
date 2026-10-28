//! Integration tests for interrupting a live turn.
//!
//! Covers `SessionsAPI::interrupt_turn`'s contract, the accounting an
//! interrupted turn records, and the single representation of the outcome:
//! `SessionError::Interrupted`, never
//! `SessionError::Execution(ExecutionError::Interrupted { .. })`.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_graph::ExecutionError;
use polaris_graph::cancel::RunCancellation;
use polaris_graph::graph::Graph;
use polaris_sessions::models::{BucketGranularity, TurnStatus, UptimeStatus};
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin};
use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// Test fixtures
// ─────────────────────────────────────────────────────────────────────────────

/// Lets the test know a turn has genuinely entered its first node, so no
/// assertion depends on wall-clock timing.
#[derive(Clone, Default)]
struct TurnGate {
    entered: Arc<tokio::sync::Notify>,
    /// Bodies that ran, across all turns of the session.
    bodies: Arc<AtomicU32>,
    /// Set if the node after the parking one ever starts.
    second_node_ran: Arc<AtomicBool>,
}

impl LocalResource for TurnGate {}

/// Parks until the run is interrupted, then returns normally.
///
/// Reads the token rather than sleeping so the test is deterministic: the
/// turn ends when, and only when, `interrupt_turn` is called.
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

/// Two nodes: the first parks on the signal, the second must never start
/// once the signal is raised.
struct InterruptibleAgent;

impl Agent for InterruptibleAgent {
    fn build(&self, graph: &mut Graph) {
        graph
            .add_system(park_until_interrupted)
            .add_system(second_node);
    }

    fn name(&self) -> &'static str {
        "InterruptibleAgent"
    }
}

/// Completes immediately. Used to prove a session still works after an
/// interrupt, and that an idle-time raise was not latched.
#[system]
async fn quick_step(gate: Res<TurnGate>) -> u32 {
    gate.bodies.fetch_add(1, Ordering::SeqCst) + 1
}

struct QuickAgent;

impl Agent for QuickAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(quick_step);
    }

    fn name(&self) -> &'static str {
        "QuickAgent"
    }
}

/// Builds a server with sessions and auto-checkpoint left **on**, since the
/// checkpoint an interrupted turn takes is part of what is under test.
async fn test_server() -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.register_agent(InterruptibleAgent).unwrap();
    sessions.register_agent(QuickAgent).unwrap();

    server
}

fn create_session(server: &Server, id: &SessionId, agent: &'static str, gate: &TurnGate) {
    let sessions = server.api::<SessionsAPI>().unwrap();
    let gate = gate.clone();
    sessions
        .create_session_with(
            server.create_context(),
            id,
            &AgentTypeId::from_name(agent),
            move |ctx| {
                ctx.insert(gate);
            },
        )
        .unwrap();
}

/// Returns the one live session that is not in `known` — the ephemeral session
/// a `run_oneshot` is currently driving. Taking the first live session would
/// be wrong whenever the test already owns one.
#[track_caller]
fn ephemeral_session(sessions: &SessionsAPI, known: &[SessionId]) -> SessionId {
    let mut fresh: Vec<SessionId> = sessions
        .list_live_sessions()
        .into_iter()
        .filter(|id| !known.contains(id))
        .collect();
    assert_eq!(
        fresh.len(),
        1,
        "exactly one ephemeral session should be live, found {fresh:?}"
    );
    fresh.remove(0)
}

/// Unwraps a `SessionError::Interrupted`, failing with the actual error
/// otherwise — and failing loudly on the wrapped shape, which an interrupt must
/// never produce.
#[track_caller]
fn expect_interrupted(err: SessionError) -> (u32, usize) {
    match err {
        SessionError::Interrupted {
            turn,
            nodes_executed,
            ..
        } => (turn, nodes_executed),
        SessionError::Execution(ExecutionError::Interrupted { .. }) => panic!(
            "an interrupted turn must never surface as \
             SessionError::Execution(ExecutionError::Interrupted {{ .. }}) — \
             execute_turn is the sole conversion point"
        ),
        other => panic!("expected SessionError::Interrupted, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Interrupting a live turn
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn interrupting_a_live_turn_records_it_and_returns_the_session_to_idle() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });

    // The turn now holds the session's context lock for its whole duration.
    gate.entered.notified().await;

    let started = Instant::now();
    let was_running = sessions
        .interrupt_turn(&id)
        .expect("the session exists, so the raise reaches it");
    let raise_latency = started.elapsed();

    assert!(was_running, "a turn was in flight");
    // If `interrupt_turn` took the context lock this call could not return at
    // all: the turn holds it and only this call can end the turn. The bound is
    // a diagnostic, not the proof — the proof is that the test terminates.
    assert!(
        raise_latency < Duration::from_secs(1),
        "interrupt_turn must never wait for the session's context lock, took {raise_latency:?}"
    );

    let err = turn
        .await
        .expect("the turn task must not panic")
        .expect_err("an interrupted turn does not return a result");
    let (turn_number, nodes_executed) = expect_interrupted(err);

    assert_eq!(turn_number, 0, "turn 0 is the one that was interrupted");
    assert_eq!(
        nodes_executed, 1,
        "the parking node ran; the one after it was declined"
    );
    assert!(
        !gate.second_node_ran.load(Ordering::SeqCst),
        "the node after the stop must never start"
    );

    let info = sessions.session_info(&id).unwrap();
    assert_eq!(
        info.turn_number, 1,
        "the number advances: (session, turn) is the run's identity, and two runs must never share it"
    );
    assert!(
        sessions.list_checkpoints(&id).unwrap().contains(&0),
        "the post-interrupt state is consistent by construction, so it is a legitimate rollback target"
    );
}

#[tokio::test]
async fn an_interrupted_turn_is_recorded_as_interrupted_and_the_session_goes_idle() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let since = chrono::Utc::now() - chrono::Duration::minutes(1);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;
    assert!(sessions.interrupt_turn(&id).unwrap());
    expect_interrupted(turn.await.unwrap().unwrap_err());

    let history = sessions.turn_history(&id, false).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].status,
        TurnStatus::Interrupted,
        "an interrupt is a user exercising control, not a system failing"
    );
    assert!(
        history[0].finished_at.is_some(),
        "the turn record always reaches a terminal state; no turn is left Running"
    );

    // The bucket holding the turn reports `Active` by construction (any
    // `Active` transition in the window wins); the buckets after it report the
    // projected state, which is what "the session returned to idle" means.
    let uptime = sessions
        .uptime_buckets(
            &id,
            BucketGranularity::OneMinute,
            since,
            chrono::Utc::now() + chrono::Duration::minutes(3),
        )
        .unwrap();
    assert_eq!(
        uptime.buckets.last().map(|bucket| bucket.status),
        Some(UptimeStatus::Idle),
        "the session returns to idle, not stuck Active; buckets = {:?}",
        uptime.buckets
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Idle and unknown sessions
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn raising_while_idle_reports_false_and_is_not_latched_into_the_next_turn() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "QuickAgent", &gate);

    assert!(
        !sessions.interrupt_turn(&id).unwrap(),
        "no turn was in flight"
    );

    // A stray interrupt must not kill the next request.
    let result = sessions
        .process_turn(&id)
        .await
        .expect("the next turn runs untouched");
    assert_eq!(result.nodes_executed(), 1);
    assert_eq!(gate.bodies.load(Ordering::SeqCst), 1);
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 1);
}

#[tokio::test]
async fn raising_for_an_unknown_session_reports_session_not_found() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();

    let err = sessions
        .interrupt_turn(&SessionId::new())
        .expect_err("an unknown session is not a silent no-op");
    assert!(
        matches!(err, SessionError::SessionNotFound(_)),
        "got {err:?}"
    );
}

/// The counter-test to the dropped-future case below: while a turn is genuinely
/// in flight the slot is open, so the raise reports `Ok(true)` and the record is
/// still `Running`. A guard that closed the window too early would pass the
/// dropped-future test and fail this one.
#[tokio::test]
async fn a_turn_in_flight_reports_true_while_its_record_is_still_running() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;

    let history = sessions.turn_history(&id, false).unwrap();
    assert_eq!(history.len(), 1, "the in-flight turn is recorded");
    assert_eq!(
        history[0].status,
        TurnStatus::Running,
        "the record stays Running for as long as the turn holds the slot"
    );
    assert!(
        history[0].finished_at.is_none(),
        "an in-flight turn has not finished"
    );
    assert!(
        sessions.interrupt_turn(&id).unwrap(),
        "the slot is open for the whole executor call, so the raise finds the turn"
    );

    expect_interrupted(turn.await.unwrap().unwrap_err());
}

/// Dropping the turn future — what axum does to the `POST /turns` handler when
/// the client disconnects — must close the interrupt slot, finalize the turn
/// record and return the session to idle.
///
/// Without a drop guard the slot stays `Some(dead_source)`, so `interrupt_turn`
/// answers `Ok(true)` for an idle session and a host running the documented
/// "one press interrupts, a second quits" escalation quits on the first press.
#[tokio::test]
async fn dropping_the_turn_future_closes_the_slot_and_finalizes_the_record() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let since = chrono::Utc::now() - chrono::Duration::minutes(1);

    // Drive the turn until its first node has genuinely parked on the signal,
    // then drop the future. `select!` rather than a timeout keeps the drop
    // point deterministic: the parking node never returns on its own.
    {
        let mut turn = Box::pin(sessions.process_turn(&id));
        tokio::select! {
            _ = &mut turn => panic!("the parking node only returns once the run is cancelled"),
            () = gate.entered.notified() => {}
        }
    }
    assert_eq!(
        gate.bodies.load(Ordering::SeqCst),
        1,
        "the turn really was in flight when it was dropped"
    );

    assert!(
        !sessions.interrupt_turn(&id).unwrap(),
        "an abandoned turn leaves no live cancellation authority behind"
    );

    let history = sessions.turn_history(&id, false).unwrap();
    assert_eq!(history.len(), 1, "the abandoned turn is still recorded");
    assert_eq!(
        history[0].status,
        TurnStatus::Failed,
        "an abandoned turn is no longer Running; it advanced no turn number and \
         took no checkpoint, so it is Failed rather than Interrupted"
    );
    assert!(
        history[0].finished_at.is_some(),
        "the record reaches a terminal state with a finish timestamp"
    );

    let uptime = sessions
        .uptime_buckets(
            &id,
            BucketGranularity::OneMinute,
            since,
            chrono::Utc::now() + chrono::Duration::minutes(3),
        )
        .unwrap();
    assert_eq!(
        uptime.buckets.last().map(|bucket| bucket.status),
        Some(UptimeStatus::Idle),
        "the session is idle again, not stuck Active; buckets = {:?}",
        uptime.buckets
    );

    // And the session is genuinely usable: the next turn runs untouched by the
    // abandoned one's signal.
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let next = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;
    assert!(sessions.interrupt_turn(&id).unwrap());
    let (turn_number, _) = expect_interrupted(next.await.unwrap().unwrap_err());
    assert_eq!(
        turn_number, 0,
        "the abandoned turn consumed no turn number, so the retry is still turn 0"
    );
}

#[tokio::test]
async fn a_second_raise_during_the_same_turn_also_reports_true() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;

    assert!(sessions.interrupt_turn(&id).unwrap());
    // Escalation policy — "a second press quits" — belongs to the host and its
    // own timer. This method stays idempotent.
    assert!(sessions.interrupt_turn(&id).unwrap());

    expect_interrupted(turn.await.unwrap().unwrap_err());
}

/// Idempotence is about the *accounting*, not just the return value: two raises
/// inside one turn must leave exactly one history entry, one finish timestamp,
/// one advanced turn number and one checkpoint. Once the turn ends the slot is
/// closed, so a third raise reports `Ok(false)`.
#[tokio::test]
async fn a_double_raise_does_not_double_finalize_the_turn_record() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    // Session creation already took a boundary checkpoint, so count the delta.
    let checkpoints_before = sessions.list_checkpoints(&id).unwrap().len();

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;

    assert!(sessions.interrupt_turn(&id).unwrap());
    assert!(sessions.interrupt_turn(&id).unwrap());

    let (turn_number, _) = expect_interrupted(turn.await.unwrap().unwrap_err());
    assert_eq!(turn_number, 0);

    assert!(
        !sessions.interrupt_turn(&id).unwrap(),
        "the slot closes when the turn ends, so a late raise finds nothing"
    );

    let history = sessions.turn_history(&id, false).unwrap();
    assert_eq!(
        history.len(),
        1,
        "two raises produced one turn, not two records: {history:?}"
    );
    assert_eq!(history[0].turn, 0);
    assert_eq!(history[0].status, TurnStatus::Interrupted);
    assert!(history[0].finished_at.is_some());
    assert_eq!(
        sessions.session_info(&id).unwrap().turn_number,
        1,
        "the number advanced once, not once per raise"
    );
    let labels = sessions.list_checkpoints(&id).unwrap();
    assert_eq!(
        labels.len(),
        checkpoints_before + 1,
        "one interrupted turn takes one checkpoint, whichever raise ended it: {labels:?}"
    );
    assert!(labels.contains(&0), "captured under the turn that ran");
}

// ─────────────────────────────────────────────────────────────────────────────
// The turn after an interrupt
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_next_turn_after_an_interrupt_gets_a_fresh_signal_and_completes() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;
    assert!(sessions.interrupt_turn(&id).unwrap());
    expect_interrupted(turn.await.unwrap().unwrap_err());

    // Turn 1 starts with a source of its own. If the previous turn's raise
    // leaked into it, the parking node would be declined before it ever ran.
    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let second = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    gate.entered.notified().await;
    assert_eq!(
        gate.bodies.load(Ordering::SeqCst),
        2,
        "the second turn's first node really did start"
    );
    assert!(sessions.interrupt_turn(&id).unwrap());

    let (turn_number, _) = expect_interrupted(second.await.unwrap().unwrap_err());
    assert_eq!(
        turn_number, 1,
        "the second turn is turn 1, not turn 0 again"
    );
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 2);
    assert!(
        sessions.list_checkpoints(&id).unwrap().contains(&1),
        "each interrupted turn checkpoints under its own number"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The try-lock entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn try_process_turn_with_records_the_same_accounting() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = SessionId::new();
    let gate = TurnGate::default();
    create_session(&server, &id, "InterruptibleAgent", &gate);

    let turn_sessions = sessions.clone();
    let turn_id = id.clone();
    let turn =
        tokio::spawn(async move { turn_sessions.try_process_turn_with(&turn_id, |_| {}).await });
    gate.entered.notified().await;
    assert!(sessions.interrupt_turn(&id).unwrap());

    let (turn_number, nodes_executed) = expect_interrupted(turn.await.unwrap().unwrap_err());
    assert_eq!(turn_number, 0);
    assert_eq!(nodes_executed, 1);
    assert_eq!(sessions.session_info(&id).unwrap().turn_number, 1);
    assert!(sessions.list_checkpoints(&id).unwrap().contains(&0));
}

// ─────────────────────────────────────────────────────────────────────────────
// The one-shot entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_oneshot_propagates_the_interrupt_and_still_deletes_the_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let gate = TurnGate::default();

    let live_before = sessions.list_live_sessions().len();

    let oneshot_sessions = sessions.clone();
    let oneshot_gate = gate.clone();
    let oneshot = tokio::spawn(async move {
        oneshot_sessions
            .run_oneshot::<u32>(&AgentTypeId::from_name("InterruptibleAgent"), move |ctx| {
                ctx.insert(oneshot_gate);
            })
            .await
    });

    gate.entered.notified().await;
    // The ephemeral session's id is never handed to the caller, so reach it
    // through the live map.
    let ephemeral = ephemeral_session(sessions, &[]);
    assert!(sessions.interrupt_turn(&ephemeral).unwrap());

    let err = oneshot
        .await
        .unwrap()
        .expect_err("an interrupted one-shot yields no output");
    expect_interrupted(err);

    assert_eq!(
        sessions.list_live_sessions().len(),
        live_before,
        "cleanup runs on the interrupted path too; the ephemeral session is gone"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// One representation of the outcome, on every entry point
// ─────────────────────────────────────────────────────────────────────────────

/// `SessionError::Execution` carries `#[from] ExecutionError`, so a bare `?`
/// anywhere on these paths would silently produce
/// `Execution(Interrupted { .. })` — a second wire shape consumers would have
/// to match twice. All three turn entry points funnel through `execute_turn`
/// but return through different callers, so all three are checked.
#[tokio::test]
async fn no_turn_entry_point_produces_the_wrapped_interrupted_shape() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let mut known: Vec<SessionId> = Vec::new();

    // process_turn_with
    {
        let id = SessionId::new();
        known.push(id.clone());
        let gate = TurnGate::default();
        create_session(&server, &id, "InterruptibleAgent", &gate);
        let turn_sessions = sessions.clone();
        let turn_id = id.clone();
        let turn =
            tokio::spawn(async move { turn_sessions.process_turn_with(&turn_id, |_| {}).await });
        gate.entered.notified().await;
        assert!(sessions.interrupt_turn(&id).unwrap());
        assert_not_wrapped(turn.await.unwrap().unwrap_err(), "process_turn_with");
    }

    // try_process_turn_with
    {
        let id = SessionId::new();
        known.push(id.clone());
        let gate = TurnGate::default();
        create_session(&server, &id, "InterruptibleAgent", &gate);
        let turn_sessions = sessions.clone();
        let turn_id = id.clone();
        let turn =
            tokio::spawn(
                async move { turn_sessions.try_process_turn_with(&turn_id, |_| {}).await },
            );
        gate.entered.notified().await;
        assert!(sessions.interrupt_turn(&id).unwrap());
        assert_not_wrapped(turn.await.unwrap().unwrap_err(), "try_process_turn_with");
    }

    // run_oneshot
    {
        let gate = TurnGate::default();
        let oneshot_sessions = sessions.clone();
        let oneshot_gate = gate.clone();
        let oneshot = tokio::spawn(async move {
            oneshot_sessions
                .run_oneshot::<u32>(&AgentTypeId::from_name("InterruptibleAgent"), move |ctx| {
                    ctx.insert(oneshot_gate);
                })
                .await
        });
        gate.entered.notified().await;
        let ephemeral = ephemeral_session(sessions, &known);
        assert!(sessions.interrupt_turn(&ephemeral).unwrap());
        assert_not_wrapped(oneshot.await.unwrap().unwrap_err(), "run_oneshot");
    }
}

#[track_caller]
fn assert_not_wrapped(err: SessionError, entry_point: &str) {
    assert!(
        !matches!(
            err,
            SessionError::Execution(ExecutionError::Interrupted { .. })
        ),
        "{entry_point} produced the forbidden wrapped shape"
    );
    assert!(
        matches!(err, SessionError::Interrupted { .. }),
        "{entry_point} must return SessionError::Interrupted, got {err:?}"
    );
}
