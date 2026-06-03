//! Pins that [`SpanStorePlugin::cleanup`] drains records still queued at
//! shutdown into the store, exercising the plugin's `cleanup()` lifecycle
//! method end-to-end through `Server::cleanup` — not just the bare
//! `run_writer` drain helper covered by the unit tests.
//!
//! Lives in its own test binary so the globally-installed tracing
//! subscriber does not collide with other live-subscriber tests in the
//! suite (a process can install the global default only once).

#![cfg(feature = "file-store")]

use polaris_core_plugins::{FileSpanStore, SpanStore, SpanStorePlugin, TracingPlugin};
use polaris_system::server::Server;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_drains_records_queued_before_shutdown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store: Arc<dyn SpanStore> = Arc::new(FileSpanStore::new(dir.path()));

    // `TracingPlugin::default_dependencies` auto-registers the ServerInfo /
    // Models / Tools satellites. Under the `dashboard` feature it additionally
    // requires `AppPlugin` (not auto-registered, since it needs explicit
    // host/port config), so add it explicitly to keep the server complete.
    let mut server = Server::new();
    server.add_plugins(TracingPlugin::new());
    #[cfg(feature = "dashboard")]
    server.add_plugins(polaris_app::AppPlugin::new(
        polaris_app::AppConfig::new().with_host("127.0.0.1"),
    ));
    server.add_plugins(SpanStorePlugin::new(store.clone()));
    server.finish().await.unwrap();

    // Emit a burst of session-labeled records through the live subscriber,
    // then shut the server down *immediately* — without polling the store
    // first — so the drain inside `cleanup()` is what guarantees they land.
    // The label is set on an entered span and inherited by every event and
    // by the span-close record itself.
    const EMITTED: usize = 50;
    {
        let span = tracing::info_span!("drain.test", polaris.label.session_id = "sess-drain");
        let _guard = span.enter();
        for i in 0..EMITTED {
            tracing::info!(i, "drain event");
        }
    }

    // `Server::cleanup` invokes `SpanStorePlugin::cleanup`, which signals the
    // writer to drain its queue and awaits it within the grace window.
    server.cleanup().await;

    // After a clean shutdown, every record emitted before it must be durable.
    // (`EMITTED` events plus the span-close record, so at least `EMITTED`.)
    let records = store.load("sess-drain").await.expect("load");
    assert!(
        records.len() >= EMITTED,
        "cleanup() must drain every queued record to the store; \
         emitted {EMITTED} events but only {} persisted",
        records.len()
    );
}
