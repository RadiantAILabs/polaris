//! Pins the cross-restart contract: when a [`SpanStore`] is configured,
//! the span/run history produced by one process is queryable from a fresh
//! [`SpanBuffer`] after restart.
//!
//! Regression context: `SessionStore::load` resurrects session identity and
//! resources across reboot, but the tracing-dashboard's [`SpanBuffer`] was
//! an in-memory ring with no persistence companion. The dashboard's
//! `sessions-runs` panel would return "No runs recorded for this
//! selection" against any resumed session — even one whose
//! `turn_number > 0`. [`SpanStorePlugin`] closes that gap; this test pins
//! the contract that closes it.

#![cfg(all(feature = "dashboard", feature = "file-store"))]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    FileSpanStore, ServerInfoPlugin, SpanBuffer, SpanKind, SpanRecord, SpanStore, SpanStorePlugin,
    TracingPlugin, TreeView,
};
use polaris_models::ModelsPlugin;
use polaris_system::server::Server;
use polaris_tools::ToolsPlugin;
use std::sync::Arc;

fn record_for(session: &str, run: &str, name: &str, kind: SpanKind) -> SpanRecord {
    SpanRecord::new(
        "2026-05-17T00:00:00.000Z",
        "info",
        "polaris.graph",
        name,
        kind,
    )
    .with_started_at("2026-05-17T00:00:00.000Z")
    .with_duration_ms(5)
    .with_span_id(format!("{name}-id"))
    .with_run_id(run)
    .with_label("session_id", session)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn span_buffer_rehydrates_session_runs_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    // First "process": write the history a previous run produced. Skipping
    // the server build step keeps the global tracing subscriber state
    // clean for the second "process" below — exercising the store
    // contract directly is what hydration actually depends on.
    {
        let pre = FileSpanStore::new(dir.path());
        pre.append(
            "demo-warm",
            &record_for(
                "demo-warm",
                "run-1",
                "polaris.graph.execute",
                SpanKind::SpanClose,
            ),
        )
        .await
        .unwrap();
        pre.append(
            "demo-warm",
            &record_for(
                "demo-warm",
                "run-2",
                "polaris.graph.execute",
                SpanKind::SpanClose,
            ),
        )
        .await
        .unwrap();
    }

    // Second "process": fresh server, fresh in-memory SpanBuffer, same
    // FileSpanStore. After `ready()`, the buffer must be hydrated so the
    // dashboard queries that drive the sessions-runs panel return the
    // history produced by the previous process.
    let store: Arc<dyn SpanStore> = Arc::new(FileSpanStore::new(dir.path()));
    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new())
        .add_plugins(SpanStorePlugin::new(store));
    server.finish().await.unwrap();

    let buffer = server
        .api::<SpanBuffer>()
        .expect("SpanBuffer must be present")
        .clone();

    let runs = buffer.distinct_runs_by_label("session_id", "demo-warm", usize::MAX);
    assert_eq!(
        runs.len(),
        2,
        "hydrated SpanBuffer must surface both stored runs for resumed session"
    );
    let mut ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["run-1", "run-2"]);

    // And run_tree must succeed for any of them — that's the panel the
    // operator opens to diagnose the resumed session.
    let tree = buffer
        .run_tree("run-1", TreeView::Structure)
        .expect("run_tree should return a tree for a hydrated run");
    assert_eq!(tree.run_id, "run-1");
    assert_eq!(
        tree.labels.get("session_id").map(String::as_str),
        Some("demo-warm"),
        "hydrated tree must preserve session_id label",
    );
}
