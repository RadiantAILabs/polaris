//! Session history and channel IO work without the HTTP feature.
//!
//! Every test here builds a plain `SessionsPlugin` server — no `sessions-http`,
//! no `AppPlugin` — so a failure names the un-gated behaviour that regressed
//! rather than the HTTP layer around it.

use chrono::Utc;
use polaris_agent::Agent;
use polaris_core_plugins::{IOContent, IOMessage, PersistencePlugin, UserIO};
use polaris_graph::Graph;
use polaris_sessions::{
    AgentTypeId, BucketGranularity, ChannelIOProvider, InMemoryStore, SessionError, SessionId,
    SessionUptimeResponse, SessionsAPI, SessionsPlugin, Turn, TurnNumber, TurnStatus, TurnSummary,
    models::UptimeStatus,
};
use polaris_system::{param::Res, server::Server, system};
use std::sync::Arc;

#[system]
async fn echo(io: Res<UserIO>) {
    let input = io.receive().await.unwrap();
    io.send(input).await.unwrap();
}

struct Echo;
impl Agent for Echo {
    fn name(&self) -> &'static str {
        "history-echo"
    }
    fn build(&self, graph: &mut Graph) {
        graph.add_system(echo);
    }
}

/// A sessions-only server plus one live session of the echo agent.
async fn session_fixture() -> (Server, SessionsAPI, SessionId) {
    let mut server = Server::new();
    server
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())).without_auto_checkpoint());
    server.finish().await.unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    sessions.register_agent(Echo).unwrap();
    let id = SessionId::new();
    sessions
        .create_session(
            server.create_context(),
            &id,
            &AgentTypeId::from_name("history-echo"),
        )
        .unwrap();
    (server, sessions, id)
}

/// Runs one turn over caller-owned channels — the transport-neutral path the
/// `sessions-http` handlers also take — and returns the recorded turn number
/// with the message the agent echoed back.
async fn run_echo_turn(
    sessions: &SessionsAPI,
    id: &SessionId,
    text: &str,
) -> (TurnNumber, IOMessage) {
    let (provider, input, mut output) = ChannelIOProvider::new(1, 1);
    input.send(IOMessage::user_text(text)).await.unwrap();
    sessions
        .process_turn_with(id, move |ctx| {
            ctx.insert(UserIO::new(Arc::new(provider)));
        })
        .await
        .unwrap();
    let summaries = sessions.turn_history(id, false).unwrap();
    let turn = summaries.last().expect("the turn was recorded").turn;
    (
        turn,
        output.try_recv().expect("the agent echoed one message"),
    )
}

#[tokio::test]
async fn history_is_empty_before_the_first_turn() {
    let (_server, sessions, id) = session_fixture().await;

    assert!(
        sessions.turn_history(&id, true).unwrap().is_empty(),
        "a session that has run no turns has no history"
    );
}

#[tokio::test]
async fn an_unrecorded_turn_reports_turn_not_found() {
    let (_server, sessions, id) = session_fixture().await;

    let err = sessions
        .turn(&id, 1)
        .expect_err("turn 1 was never run on a fresh session");
    assert!(matches!(err, SessionError::TurnNotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn a_channel_driven_turn_is_recorded_as_completed() {
    let (_server, sessions, id) = session_fixture().await;

    let (turn, echoed) = run_echo_turn(&sessions, &id, "retained message").await;
    assert!(
        matches!(&echoed.content, IOContent::Text(text) if text == "retained message"),
        "the agent echoed the input back through the channel: {echoed:?}"
    );

    let summaries: Vec<TurnSummary> = sessions.turn_history(&id, false).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].turn, turn);
    assert_eq!(summaries[0].status, TurnStatus::Completed);
    assert!(
        summaries[0].messages.is_none(),
        "messages are omitted unless the caller asks for them"
    );
    assert_eq!(
        summaries[0].io_message_count, 0,
        "nothing is recorded until a caller records it — the IO provider belongs \
         to the caller, not to the session"
    );
}

#[tokio::test]
async fn record_turn_messages_attaches_io_to_the_turn() {
    let (_server, sessions, id) = session_fixture().await;
    let (turn, echoed) = run_echo_turn(&sessions, &id, "retained message").await;

    sessions.record_turn_messages(&id, turn, vec![echoed]);

    let record: Turn = sessions.turn(&id, turn).unwrap();
    assert_eq!(record.status, TurnStatus::Completed);
    assert!(
        record.finished_at.is_some(),
        "a completed turn carries a finish timestamp"
    );
    assert!(
        matches!(&record.messages[0].content, IOContent::Text(text) if text == "retained message")
    );

    let summaries = sessions.turn_history(&id, true).unwrap();
    assert_eq!(summaries[0].io_message_count, 1);
    assert_eq!(summaries[0].messages.as_ref().unwrap().len(), 1);
}

/// Recording stays bounded even for callers that hand over oversized captures:
/// the retained copy is truncated and the record says so.
#[tokio::test]
async fn an_oversized_recorded_message_is_truncated_and_flagged() {
    let (_server, sessions, id) = session_fixture().await;
    let (turn, _echoed) = run_echo_turn(&sessions, &id, "retained message").await;

    sessions.record_turn_messages(&id, turn, vec![IOMessage::user_text("x".repeat(70_000))]);

    let record = sessions.turn(&id, turn).unwrap();
    assert!(
        record.messages_truncated,
        "the record must admit that it dropped payload"
    );
    assert!(
        matches!(&record.messages[0].content, IOContent::Text(text) if text.len() <= 64 * 1024),
        "the retained copy is capped at 64 KiB"
    );
}

/// A bucket reports `Active` when a turn ran inside it and otherwise the state
/// projected from the last transition, so a window that outlasts the turn
/// shows `Active` for the turn and `Idle` once it has finished.
#[tokio::test]
async fn uptime_is_bucketed_without_http() {
    let since = Utc::now();
    let (_server, sessions, id) = session_fixture().await;
    let _ = run_echo_turn(&sessions, &id, "retained message").await;

    let uptime: SessionUptimeResponse = sessions
        .uptime_buckets(
            &id,
            BucketGranularity::OneMinute,
            since,
            since + chrono::Duration::minutes(3),
        )
        .unwrap();
    assert_eq!(
        uptime
            .buckets
            .iter()
            .map(|bucket| bucket.status)
            .collect::<Vec<_>>(),
        vec![UptimeStatus::Active, UptimeStatus::Idle, UptimeStatus::Idle],
        "the turn's minute is Active and the session is Idle once it ends: {:?}",
        uptime.buckets
    );
}

/// The control for the test above: a session that has run no turn reports
/// `Idle` throughout, so the `Active` bucket there comes from the turn.
#[tokio::test]
async fn an_unused_session_reports_idle_uptime() {
    let since = Utc::now();
    let (_server, sessions, id) = session_fixture().await;

    let uptime = sessions
        .uptime_buckets(
            &id,
            BucketGranularity::OneMinute,
            since,
            since + chrono::Duration::minutes(2),
        )
        .unwrap();
    assert_eq!(
        uptime
            .buckets
            .iter()
            .map(|bucket| bucket.status)
            .collect::<Vec<_>>(),
        vec![UptimeStatus::Idle, UptimeStatus::Idle],
        "{:?}",
        uptime.buckets
    );
}

#[tokio::test]
async fn deleting_a_session_drops_its_history() {
    let (_server, sessions, id) = session_fixture().await;
    let _ = run_echo_turn(&sessions, &id, "retained message").await;
    assert_eq!(sessions.turn_history(&id, false).unwrap().len(), 1);

    sessions.delete_session(&id).await.unwrap();

    let err = sessions
        .turn_history(&id, false)
        .expect_err("a deleted session is not queryable");
    assert!(
        matches!(err, SessionError::SessionNotFound(_)),
        "got {err:?}"
    );
}
