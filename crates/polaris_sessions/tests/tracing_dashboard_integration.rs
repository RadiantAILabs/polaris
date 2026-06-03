//! End-to-end: when [`SessionsPlugin`] runs a turn against an agent, the
//! tracing-dashboard [`SpanBuffer`] captures the resulting `polaris.run`
//! and middleware-installed `polaris.graph.execute_system` spans, and the
//! per-session run summary lookup surfaces the run.
//!
//! Regression context: a downstream consumer reported that none of these
//! spans reached the buffer in their dashboard build. The
//! `tracing_executor_repro` test in `polaris_core_plugins` already pins
//! the executor → buffer contract by driving `GraphExecutor` directly;
//! this test pins the higher-level `SessionsAPI::process_turn` path, so a
//! future wiring break between `SessionsPlugin::ready()` and
//! `TracingPlugin`'s `MiddlewareAPI` will fail here loudly.

use polaris_agent::Agent;
use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::persistence::PersistencePlugin;
use polaris_core_plugins::{ServerInfoPlugin, SpanBuffer, SpanKind, TracingPlugin};
use polaris_graph::graph::Graph;
use polaris_models::ModelsPlugin;
use polaris_sessions::store::memory::InMemoryStore;
use polaris_sessions::store::{AgentTypeId, SessionId};
use polaris_sessions::{SessionsAPI, SessionsPlugin};
use polaris_system::server::Server;
use polaris_system::system;
use polaris_tools::ToolsPlugin;
use std::sync::Arc;

#[system]
async fn noop_step() {}

struct NoopAgent;

impl Agent for NoopAgent {
    fn build(&self, graph: &mut Graph) {
        graph.add_system(noop_step);
    }

    fn name(&self) -> &'static str {
        "NoopAgent"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_turn_emits_spans_to_dashboard_span_buffer() {
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
    server.finish().await.unwrap();

    let sessions = server
        .api::<SessionsAPI>()
        .expect("SessionsAPI registered")
        .clone();
    sessions.register_agent(NoopAgent).expect("register agent");

    let id = SessionId::from_string("integration-test");
    let agent_type = AgentTypeId::from_name("NoopAgent");
    sessions
        .create_session(server.create_context(), &id, &agent_type)
        .expect("create session");
    let result = sessions
        .process_turn(&id)
        .await
        .expect("process_turn should succeed");
    assert_eq!(
        result.nodes_executed(),
        1,
        "graph must have actually run noop_step — a short-circuited turn would still emit a polaris.run close but not advance the executor",
    );

    let buffer = server
        .api::<SpanBuffer>()
        .expect("SpanBuffer registered")
        .clone();
    let snapshot = buffer.snapshot(usize::MAX);

    let polaris_run_closes = snapshot
        .iter()
        .filter(|record| record.kind == SpanKind::SpanClose && record.name == "polaris.run")
        .count();
    assert_eq!(
        polaris_run_closes, 1,
        "exactly one polaris.run close expected for a single process_turn invocation (got {polaris_run_closes})",
    );

    assert!(
        snapshot
            .iter()
            .any(|record| record.kind == SpanKind::SpanClose
                && record.name == "polaris.graph.execute_system"),
        "graph_tracing middleware's polaris.graph.execute_system close record missing"
    );
    assert!(
        snapshot
            .iter()
            .any(|record| record.kind == SpanKind::SpanClose
                && record.name == "polaris.session.turn"),
        "session.turn close record missing"
    );

    let runs = buffer.distinct_runs_by_label("session_id", id.as_str(), 10);
    assert_eq!(
        runs.len(),
        1,
        "exactly one run should be visible via /v1/sessions/{}/runs (got {:?})",
        id.as_str(),
        runs
    );
}
