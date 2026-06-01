//! Pins the contract for [`SessionsAPI::run_oneshot_preserved`]:
//! a finished one-shot session stays in the live map as read-only,
//! mutation surfaces reject, and read surfaces continue to work.

use polaris_agent::Agent;
use polaris_core_plugins::persistence::{PersistenceAPI, PersistencePlugin, Storable};
use polaris_graph::graph::Graph;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionStore};
use polaris_sessions::{SessionError, SessionStatus, SessionsAPI, SessionsPlugin};
use polaris_system::param::ResMut;
use polaris_system::resource::LocalResource;
use polaris_system::server::Server;
use polaris_system::system;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize, Storable, PartialEq)]
#[storable(key = "Counter")]
struct Counter {
    value: u32,
}
impl LocalResource for Counter {}

#[derive(Debug, Clone, PartialEq)]
struct CounterOutput {
    value: u32,
}

#[system]
async fn increment_and_emit(mut counter: ResMut<Counter>) -> CounterOutput {
    counter.value += 1;
    CounterOutput {
        value: counter.value,
    }
}

struct CounterAgent;

impl Agent for CounterAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(increment_and_emit);
    }

    fn name(&self) -> &'static str {
        "CounterAgent"
    }
}

async fn test_server() -> Server {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
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

#[tokio::test]
async fn returns_session_id_and_output() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, output): (_, CounterOutput) = sessions
        .run_oneshot_preserved(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .expect("preserved oneshot should succeed");

    assert_eq!(output, CounterOutput { value: 1 });
    assert!(
        sessions.list_live_sessions().contains(&id),
        "preserved session must remain in the live map"
    );
}

#[tokio::test]
async fn session_status_is_read_only() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    let meta = sessions.session_info(&id).unwrap();
    assert_eq!(meta.status, SessionStatus::ReadOnly);
    assert_eq!(meta.turn_number, 1);
}

#[tokio::test]
async fn process_turn_rejects_with_read_only() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    let err = sessions.process_turn(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::ReadOnly(ref got) if got == &id),
        "expected ReadOnly, got {err:?}"
    );

    // try_process_turn must reject for the same reason — without acquiring
    // the ctx lock, which would otherwise mask the read-only check.
    let err = sessions.try_process_turn(&id).await.unwrap_err();
    assert!(matches!(err, SessionError::ReadOnly(_)));
}

#[tokio::test]
async fn rollback_setup_with_context_reject() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    assert!(matches!(
        sessions.rollback(&id, 0).await,
        Err(SessionError::ReadOnly(_))
    ));
    assert!(matches!(
        sessions.setup_session(&id).await,
        Err(SessionError::ReadOnly(_))
    ));
    let with_ctx_err = sessions
        .with_context(&id, |_ctx| ())
        .await
        .expect_err("with_context must reject on read-only");
    assert!(matches!(with_ctx_err, SessionError::ReadOnly(_)));
}

#[tokio::test]
async fn checkpoint_and_save_still_work() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    let turn = sessions
        .checkpoint(&id)
        .await
        .expect("checkpoint should succeed on a read-only session");
    assert_eq!(turn, 1);
    assert!(sessions.list_checkpoints(&id).unwrap().contains(&turn));

    sessions
        .save_session(&id)
        .await
        .expect("save_session should succeed on a read-only session");
    let stored = sessions.list_sessions().await.unwrap();
    assert!(stored.contains(&id));
}

#[tokio::test]
async fn delete_clears_read_only_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    sessions.delete_session(&id).await.unwrap();
    assert!(!sessions.list_live_sessions().contains(&id));
    assert!(matches!(
        sessions.session_info(&id),
        Err(SessionError::SessionNotFound(_))
    ));
}

#[tokio::test]
async fn execution_failure_cleans_up_like_run_oneshot() {
    // Driving execution without seeding Counter forces a resource-resolution
    // failure inside the system. The contract: preserved-mode does not
    // promote a broken session to read-only; it cleans up.
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let before = sessions.list_live_sessions().len();
    let result = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |_| {})
        .await;

    assert!(
        matches!(result, Err(SessionError::Execution(_))),
        "expected Execution, got {result:?}"
    );
    assert_eq!(
        sessions.list_live_sessions().len(),
        before,
        "failed preservation must not leave a session behind"
    );
}

#[tokio::test]
async fn resume_rejects_overwriting_read_only_session() {
    let server = test_server().await;
    let sessions = server.api::<SessionsAPI>().unwrap();
    let agent_type = AgentTypeId::from_name("CounterAgent");

    let (id, _) = sessions
        .run_oneshot_preserved::<CounterOutput>(&agent_type, |ctx| {
            ctx.insert(Counter::default());
        })
        .await
        .unwrap();

    // Persist so a subsequent resume call has something to load.
    sessions.save_session(&id).await.unwrap();

    let ctx = sessions.create_context();
    let err = sessions
        .resume_session(ctx, &id)
        .await
        .expect_err("resume must not silently overwrite a read-only session");
    assert!(matches!(err, SessionError::ReadOnly(ref got) if got == &id));
}
