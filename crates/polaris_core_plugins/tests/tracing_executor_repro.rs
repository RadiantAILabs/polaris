//! End-to-end check that `polaris.run` and graph-tracing middleware spans
//! emitted by [`GraphExecutor`] reach the dashboard [`SpanBuffer`] mounted
//! by [`TracingPlugin`] when the `dashboard` feature is on.
//!
//! The existing dashboard-http tests push synthetic [`SpanRecord`]s into
//! the buffer directly — they don't exercise the `tracing` subscriber path
//! at all. This test installs the subscriber via `TracingPlugin::ready()`
//! (which uses `try_init`), runs a real graph through `GraphExecutor`, and
//! asserts that both the unconditional `polaris.run` span and the
//! middleware-installed `polaris.graph.execute_system` span land in the
//! shared buffer.
//!
//! Regression context: a downstream consumer reported that neither span
//! reached the buffer in their dashboard build. This test pins the
//! polar-rs side of that contract.

#![cfg(feature = "dashboard")]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{FmtConfig, ServerInfoPlugin, SpanBuffer, SpanKind, TracingPlugin};
use polaris_graph::hooks::HooksAPI;
use polaris_graph::{Graph, GraphExecutor, MiddlewareAPI};
use polaris_system::server::Server;
use polaris_system::system;
use tracing::Instrument;

#[system]
async fn noop_step() {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executor_spans_reach_dashboard_span_buffer() {
    let mut server = Server::new();
    server.add_plugins(ServerInfoPlugin);
    server.add_plugins(polaris_models::ModelsPlugin);
    server.add_plugins(polaris_tools::ToolsPlugin);
    server
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new().with_fmt(FmtConfig::default()));
    server.finish().await.unwrap();

    let buffer = server
        .api::<SpanBuffer>()
        .expect("SpanBuffer should be present after TracingPlugin builds with `dashboard`")
        .clone();

    // 1. A manually-emitted `polaris.run` span must reach the buffer.
    //    If this fails, the subscriber wiring is broken before we even
    //    invoke the executor.
    let manual_run = tracing::info_span!("polaris.run", polaris.run.id = "manual-id");
    async {
        tracing::info!(message = "inside manual run");
    }
    .instrument(manual_run)
    .await;

    let snapshot = buffer.snapshot(usize::MAX);
    assert!(
        snapshot
            .iter()
            .any(|record| record.kind == SpanKind::SpanClose && record.name == "polaris.run"),
        "manually-emitted polaris.run should land in buffer"
    );

    // 2. TracingPlugin's `register_instrumentation` must have published a
    //    MiddlewareAPI carrying the graph-tracing middleware. SessionsAPI
    //    reads this same API during ready() and threads it into the
    //    executor — if it returns None here, the graph-tracing middleware
    //    is silently absent in downstream consumers.
    let middleware = server
        .api::<MiddlewareAPI>()
        .expect("TracingPlugin should register MiddlewareAPI when graph_tracing is enabled")
        .clone();
    let hooks = server.api::<HooksAPI>().cloned();

    // 3. Run a trivial graph through `GraphExecutor::execute`. The
    //    executor unconditionally opens a `polaris.run` span at
    //    `executor/mod.rs:672`, and the registered middleware wraps each
    //    system in `polaris.graph.execute_system`.
    let mut graph = Graph::new();
    graph.add_system(noop_step);

    let executor = GraphExecutor::new();
    let mut ctx = server.create_context();
    let result = executor
        .execute(&graph, &mut ctx, hooks.as_ref(), Some(&middleware))
        .await
        .expect("graph execute should succeed");
    assert_eq!(result.nodes_executed(), 1);

    let snapshot = buffer.snapshot(usize::MAX);

    let polaris_run_closes = snapshot
        .iter()
        .filter(|record| record.kind == SpanKind::SpanClose && record.name == "polaris.run")
        .count();
    assert_eq!(
        polaris_run_closes, 2,
        "expected exactly two polaris.run close records (manual + executor), got {polaris_run_closes}"
    );

    assert!(
        snapshot
            .iter()
            .any(|record| record.kind == SpanKind::SpanClose
                && record.name == "polaris.graph.execute_system"),
        "graph_tracing middleware should produce a polaris.graph.execute_system close record"
    );

    server.cleanup().await;
}
