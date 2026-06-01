//! Pins the contract that the tracing dashboard retains an entry for a
//! session whose lifetime ended with [`SessionsAPI::run_oneshot`].
//!
//! `run_oneshot` deletes its ephemeral session from the
//! [`SessionStore`](polaris_sessions::SessionStore) as soon as the turn
//! completes, so the live-sessions list (`GET /v1/sessions`, backed by
//! [`SessionsAPI::list_live_sessions`]) no longer surfaces it. The
//! tracing subsystem, however, keys its data on the `session_id`
//! correlation label inside [`SpanBuffer`] — independent of session-store
//! membership. [`SpanBuffer::distinct_sessions`] is the surface that
//! makes that decoupling navigable from the dashboard, so a tracing
//! sessions list entry must still be available after a one-shot run.
//!
//! Regression context: without this contract, the dashboard's
//! `sessions-detail` section had no entry point to a deleted one-shot
//! session's spans even though they remained in `SpanBuffer`. Pair with
//! [`SpanStorePlugin`] to extend the same surface across process
//! restarts (covered by `span_store_cross_restart`).

use polaris_agent::Agent;
use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_core_plugins::{ServerInfoPlugin, SpanBuffer, TracingPlugin};
use polaris_graph::graph::Graph;
use polaris_models::ModelsPlugin;
use polaris_sessions::store::AgentTypeId;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::{SessionsAPI, SessionsPlugin};
use polaris_system::server::Server;
use polaris_system::system;
use polaris_tools::ToolsPlugin;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
struct PingOutput;

#[system]
async fn ping() -> PingOutput {
    PingOutput
}

struct PingAgent;

impl Agent for PingAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(ping);
    }

    fn name(&self) -> &'static str {
        "PingAgent"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_oneshot_session_remains_visible_via_distinct_sessions() {
    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        // ModelsPlugin + ToolsPlugin are required by TracingPlugin when the
        // `models_tracing` / `tools_tracing` features are active, which the
        // `dashboard` feature implies.
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())));
    server.finish().await;

    let sessions = server
        .api::<SessionsAPI>()
        .expect("SessionsAPI registered")
        .clone();
    sessions.register_agent(PingAgent).expect("register agent");

    let agent_type = AgentTypeId::from_name("PingAgent");
    let _: PingOutput = sessions
        .run_oneshot(&agent_type, |_| {})
        .await
        .expect("run_oneshot should succeed");

    assert!(
        sessions.list_live_sessions().is_empty(),
        "run_oneshot must clean up the ephemeral session"
    );

    let buffer = server
        .api::<SpanBuffer>()
        .expect("SpanBuffer registered")
        .clone();

    let observed = buffer.distinct_sessions(10);
    assert_eq!(
        observed.len(),
        1,
        "exactly one session should remain visible via /v1/tracing/sessions \
         after run_oneshot, even though it has been removed from the sessions \
         store (got {observed:?})"
    );
    let summary = &observed[0];
    assert!(
        !summary.session_id.is_empty(),
        "the surviving entry must carry a non-empty session_id label"
    );
    assert!(
        summary.run_count >= 1,
        "the surviving entry must record at least one run, got {}",
        summary.run_count
    );
    assert_eq!(
        summary.agent_name.as_deref(),
        Some("PingAgent"),
        "the surviving entry must carry the agent_type label set by SessionsAPI"
    );
}
