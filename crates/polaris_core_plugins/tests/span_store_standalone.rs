//! Pins the composition rule that [`SpanStorePlugin`] runs without the
//! `dashboard` feature: persistence is a standalone capability, buffer
//! hydration is an opt-in enrichment when the dashboard feature is on.
//!
//! Aligns with Polaris's philosophy of small, snap-together plugins —
//! adding durability must not require pulling the entire dashboard stack
//! in alongside it.

#![cfg(all(not(feature = "dashboard"), feature = "file-store"))]

use polaris_core_plugins::{
    FileSpanStore, ServerInfoPlugin, SpanKind, SpanRecord, SpanStore, SpanStoreHandle,
    SpanStorePlugin, TracingPlugin,
};
use polaris_models::ModelsPlugin;
use polaris_system::server::Server;
use polaris_tools::ToolsPlugin;
use std::sync::Arc;

fn record_for(session: &str, run: &str) -> SpanRecord {
    SpanRecord::new(
        "2026-05-17T00:00:00.000Z",
        "info",
        "polaris.graph",
        "polaris.graph.execute",
        SpanKind::SpanClose,
    )
    .with_run_id(run)
    .with_label("session_id", session)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn span_store_plugin_runs_without_dashboard_buffer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn SpanStore> = Arc::new(FileSpanStore::new(dir.path()));

    // Seed history before boot so hydration has something to attempt.
    store
        .append("sess-A", &record_for("sess-A", "run-A"))
        .await
        .unwrap();

    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(TracingPlugin::new())
        // Deliberately *no* AppPlugin — the `dashboard` feature is off
        // for this test, so SpanBuffer is not in the public API surface.
        .add_plugins(SpanStorePlugin::new(store.clone()));
    server.finish().await.unwrap();

    // Without the `dashboard` feature, `ready()` is a no-op: the seeded
    // record must still be the only one in the store — no double-hydration,
    // no panic for missing `SpanBuffer` API. Pin the count so a future
    // change that adds an unconditional hydration path (e.g. into a
    // non-dashboard sink) fails here.
    assert_eq!(store.load("sess-A").await.unwrap().len(), 1);

    // The plugin still exposes its API to downstream consumers — a custom
    // replay UI can resolve the configured store via `SpanStoreHandle`
    // even when the dashboard feature is off.
    let handle = server
        .api::<SpanStoreHandle>()
        .expect("SpanStoreHandle should be registered regardless of dashboard feature");
    assert_eq!(
        handle.store().load("sess-A").await.unwrap().len(),
        1,
        "handle resolves the same store the plugin was constructed with"
    );

    // The store layer is wired in even without a dashboard buffer to
    // hydrate. Appending via the store handle continues to work after
    // boot — a hard contract that downstream tools (e.g. a custom replay
    // UI) can rely on without the dashboard.
    store
        .append("sess-A", &record_for("sess-A", "run-B"))
        .await
        .unwrap();
    assert_eq!(store.load("sess-A").await.unwrap().len(), 2);

    server.cleanup().await;
}
