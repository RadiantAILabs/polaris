//! What a failed turn records.
//!
//! A turn that ends in an execution error is recorded as `Failed`. Unlike a
//! completed or an interrupted turn, it neither advances the session's turn
//! number nor takes an automatic checkpoint: nothing about the failed run is a
//! state to resume from, and the next attempt reuses the same turn number.
//!
//! Auto-checkpoint stays **on** here. With it off, "no checkpoint after a
//! failure" would hold for any implementation.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_graph::ExecutionError;
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionError, SessionsAPI, SessionsPlugin, TurnStatus};
use polaris_system::param::Res;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use std::sync::Arc;

/// A resource the failing agent requires but no session ever inserts, so
/// `Res<MissingInput>` cannot resolve and the turn ends in an
/// `ExecutionError`.
#[derive(Debug, Clone)]
struct MissingInput;
impl LocalResource for MissingInput {}

#[system]
async fn needs_missing(_input: Res<MissingInput>) {}

struct FailingAgent;

impl Agent for FailingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(needs_missing);
    }

    fn name(&self) -> &'static str {
        "FailingAgent"
    }
}

/// A server with auto-checkpoint at its default (on) and a failing agent
/// registered.
async fn test_server() -> Server {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await.unwrap();
    server
        .api::<SessionsAPI>()
        .unwrap()
        .register_agent(FailingAgent)
        .unwrap();
    server
}

fn create_session(server: &Server) -> SessionId {
    let id = SessionId::new();
    server
        .api::<SessionsAPI>()
        .unwrap()
        .create_session(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("FailingAgent"),
        )
        .unwrap();
    id
}

/// Asserts the accounting of a session whose only turn has just failed, and
/// that `checkpoints_before` is still the number of retained checkpoints.
#[track_caller]
fn assert_failed_turn_accounting(
    sessions: &SessionsAPI,
    id: &SessionId,
    err: &SessionError,
    checkpoints_before: usize,
) {
    assert!(
        matches!(err, SessionError::Execution(ExecutionError::SystemError(_))),
        "a system that cannot resolve its resource fails the turn, got {err:?}"
    );

    let history = sessions.turn_history(id, false).unwrap();
    assert_eq!(history.len(), 1, "the failed turn is recorded: {history:?}");
    assert_eq!(history[0].status, TurnStatus::Failed);
    assert_eq!(history[0].turn, 0);
    assert!(
        history[0].finished_at.is_some(),
        "the record reaches a terminal state, so no turn is left Running"
    );

    assert_eq!(
        sessions.session_info(id).unwrap().turn_number,
        0,
        "a failed turn does not advance the turn number"
    );
    assert_eq!(
        sessions.checkpoints().list(id).unwrap().len(),
        checkpoints_before,
        "a failed turn takes no automatic checkpoint"
    );
}

#[tokio::test]
async fn a_failed_turn_is_recorded_failed_and_neither_advances_nor_checkpoints() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = create_session(&server);

    let checkpoints_before = sessions.checkpoints().list(&id).unwrap().len();
    assert_eq!(
        checkpoints_before, 1,
        "auto-checkpoint is on, so session creation took its boundary point"
    );

    let err = sessions.process_turn(&id).await.unwrap_err();

    assert_failed_turn_accounting(sessions, &id, &err, checkpoints_before);
}

#[tokio::test]
async fn a_failed_try_process_turn_records_the_same_accounting() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = create_session(&server);

    let checkpoints_before = sessions.checkpoints().list(&id).unwrap().len();

    let err = sessions
        .try_process_turn_with(&id, |_| {})
        .await
        .unwrap_err();

    assert_failed_turn_accounting(sessions, &id, &err, checkpoints_before);
}

/// The failed turn releases the session, so the retry fails with the agent's
/// own error rather than `SessionBusy`.
#[tokio::test]
async fn a_failed_turn_leaves_the_session_free_for_another_attempt() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let id = create_session(&server);

    sessions.process_turn(&id).await.unwrap_err();
    let err = sessions
        .try_process_turn_with(&id, |_| {})
        .await
        .unwrap_err();

    assert!(
        matches!(err, SessionError::Execution(ExecutionError::SystemError(_))),
        "the retry runs and fails on its own, got {err:?}"
    );
    assert!(
        !sessions.interrupt_turn(&id).unwrap(),
        "no turn is left in flight after a failure"
    );
}
