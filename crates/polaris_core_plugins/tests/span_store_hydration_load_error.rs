//! Pins the second half of [`SpanStorePlugin::ready()`]'s error-tolerance
//! contract: a per-session `load()` failure must be contained — the
//! offending session is skipped, but the rest of the hydration loop
//! continues so successfully-loaded sessions still reach the
//! [`SpanBuffer`].
//!
//! Without coverage, a silent regression to `?`-propagation (which
//! would abort hydration on the first bad session and starve every
//! later one) would only surface when one specific session's record file
//! corrupted in production. The existing fixture stores never return
//! `Err`, so the only way to exercise the failure path is with a
//! hand-rolled mock.
//!
//! Lives in its own integration-test file because [`TracingPlugin`]
//! installs a process-global subscriber — see the sibling
//! `span_store_hydration_list_error.rs` for the same rationale.

#![cfg(feature = "dashboard")]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    ServerInfoPlugin, SpanBuffer, SpanKind, SpanRecord, SpanStore, SpanStoreError, SpanStorePlugin,
    TracingPlugin,
};
use polaris_system::server::Server;
use polaris_system::system::BoxFuture;
use std::sync::Arc;

const GOOD_SESSION: &str = "sess-good";
const BAD_SESSION: &str = "sess-bad";

/// Marker name used on every record this mock yields, so the test can
/// distinguish hydrated records from the buffer's startup chatter
/// (e.g. `TracingPlugin initialized` info events).
const HYDRATED_NAME: &str = "test.hydrated_marker";

/// `SpanStore` that lists two sessions but fails to `load` one of them.
struct PartialLoadFailure;

impl SpanStore for PartialLoadFailure {
    fn append(
        &self,
        _session_id: &str,
        _record: &SpanRecord,
    ) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        Box::pin(async move { Ok(()) })
    }

    fn load(&self, session_id: &str) -> BoxFuture<'_, Result<Vec<SpanRecord>, SpanStoreError>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            if session_id == BAD_SESSION {
                Err(SpanStoreError::Backend(
                    format!("simulated load failure for {session_id}").into(),
                ))
            } else {
                Ok(vec![
                    SpanRecord::new(
                        "2026-05-17T00:00:00.000Z",
                        "info",
                        "tests",
                        HYDRATED_NAME,
                        SpanKind::SpanClose,
                    )
                    .with_run_id("run-from-store")
                    .with_label("session_id", &session_id),
                ])
            }
        })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, SpanStoreError>> {
        Box::pin(async move { Ok(vec![GOOD_SESSION.to_owned(), BAD_SESSION.to_owned()]) })
    }

    fn delete(&self, _session_id: &str) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        Box::pin(async move { Ok(()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_continues_past_per_session_load_error() {
    let store: Arc<dyn SpanStore> = Arc::new(PartialLoadFailure);

    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        .add_plugins(polaris_models::ModelsPlugin)
        .add_plugins(polaris_tools::ToolsPlugin)
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new())
        .add_plugins(SpanStorePlugin::new(store));
    server.finish().await;

    let buffer = server
        .api::<SpanBuffer>()
        .expect("SpanBuffer registered")
        .clone();
    let snapshot = buffer.snapshot(usize::MAX);

    let hydrated: Vec<&SpanRecord> = snapshot
        .iter()
        .filter(|record| record.name == HYDRATED_NAME)
        .collect();

    assert_eq!(
        hydrated.len(),
        1,
        "exactly one hydrated record expected — the good session's load succeeded; the bad session's must be skipped (got {hydrated:?})",
    );
    assert_eq!(
        hydrated[0].labels.get("session_id").map(String::as_str),
        Some(GOOD_SESSION),
        "the surviving hydrated record must come from the good session, not the failing one",
    );

    server.cleanup().await;
}
