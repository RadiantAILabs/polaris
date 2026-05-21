//! Pins that the layer installed by [`SpanStorePlugin`] receives records
//! through the live, globally-installed tracing subscriber — not just when
//! callers reach into the sink directly.
//!
//! This complements the dedicated cross-restart hydration test by
//! exercising the *write* side of the contract end-to-end: emit a
//! tracing span, observe it land in the durable [`SpanStore`].

#![cfg(all(feature = "dashboard", feature = "file-store"))]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    FileSpanStore, ServerInfoPlugin, SpanKind, SpanStore, SpanStorePlugin, TracingPlugin,
};
use polaris_models::ModelsPlugin;
use polaris_system::server::Server;
use polaris_tools::ToolsPlugin;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn span_store_layer_appends_through_live_tracing_subscriber() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn SpanStore> = Arc::new(FileSpanStore::new(dir.path()));

    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        .add_plugins(ModelsPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new())
        .add_plugins(SpanStorePlugin::new(store.clone()));
    server.finish().await;

    // Drive a session-labeled span through the global subscriber. The
    // RecordingLayer installed by SpanStorePlugin should pick it up,
    // route it through StoreSink, and persist to the FileSpanStore.
    {
        let span = tracing::info_span!(
            "polaris.graph.execute",
            polaris.run.id = "run-live",
            polaris.label.session_id = "sess-live",
        );
        let _g = span.enter();
        tracing::info!("inside scoped span");
    }

    // Spawned store appends are fire-and-forget; poll on a real timer so
    // slow CI runners get a clear "took too long" failure rather than an
    // exhausted-yield-budget false negative.
    let records = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let records = store.load("sess-live").await.expect("load");
            if !records.is_empty() {
                return records;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect(
        "live tracing subscriber must push records into SpanStore via the installed layer \
         within the timeout window",
    );
    assert!(
        records
            .iter()
            .any(|r| r.kind == SpanKind::SpanClose && r.name == "polaris.graph.execute"),
        "should persist the close record from the polaris.graph.execute span"
    );
}
