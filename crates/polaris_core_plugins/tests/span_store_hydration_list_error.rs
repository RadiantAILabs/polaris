//! Pins one half of [`SpanStorePlugin::ready()`]'s error-tolerance
//! contract: a `list_sessions()` failure must not panic the server.
//!
//! The hydration path runs once per server boot and logs `warn!` on
//! store errors instead of aborting. Without coverage, a silent
//! regression to `.unwrap()` or `?` would only surface in production
//! when a backend is briefly unreachable at startup. The existing
//! [`InMemorySpanStore`] / [`FileSpanStore`] fixtures never fail, so
//! exercising the error path requires a hand-rolled mock.
//!
//! Lives in its own integration-test file because [`TracingPlugin`]
//! installs a process-global subscriber: two `#[tokio::test]`s in the
//! same binary would race to set it.

#![cfg(feature = "dashboard")]

use polaris_app::{AppConfig, AppPlugin};
use polaris_core_plugins::{
    ServerInfoPlugin, SpanRecord, SpanStore, SpanStoreError, SpanStoreHandle, SpanStorePlugin,
    TracingPlugin,
};
use polaris_system::server::Server;
use polaris_system::system::BoxFuture;
use std::sync::Arc;

/// `SpanStore` whose `list_sessions()` always fails. Mirrors a backend
/// that is unreachable at startup.
struct FailingListSessions;

impl SpanStore for FailingListSessions {
    fn append(
        &self,
        _session_id: &str,
        _record: &SpanRecord,
    ) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        Box::pin(async move { Ok(()) })
    }

    fn load(&self, _session_id: &str) -> BoxFuture<'_, Result<Vec<SpanRecord>, SpanStoreError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<String>, SpanStoreError>> {
        Box::pin(async move {
            Err(SpanStoreError::Backend(
                "simulated list_sessions failure".into(),
            ))
        })
    }

    fn delete(&self, _session_id: &str) -> BoxFuture<'_, Result<(), SpanStoreError>> {
        Box::pin(async move { Ok(()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_tolerates_list_sessions_error() {
    let store: Arc<dyn SpanStore> = Arc::new(FailingListSessions);

    let mut server = Server::new();
    server
        .add_plugins(ServerInfoPlugin)
        .add_plugins(polaris_models::ModelsPlugin)
        .add_plugins(polaris_tools::ToolsPlugin)
        .add_plugins(AppPlugin::new(AppConfig::new().with_host("127.0.0.1")))
        .add_plugins(TracingPlugin::new())
        .add_plugins(SpanStorePlugin::new(store));

    // Core contract: `server.finish()` returns. A regression to
    // `list_sessions().unwrap()` (or `?` against the outer `Result`)
    // would panic the runtime here.
    server.finish().await;

    // Sanity: the plugin still publishes its API even when hydration
    // bails — downstream consumers that resolve `SpanStoreHandle` must
    // not crash because the backend was briefly unhealthy.
    let handle = server
        .api::<SpanStoreHandle>()
        .expect("SpanStoreHandle should be registered despite list_sessions error");
    assert!(
        handle.store().list_sessions().await.is_err(),
        "resolved handle should round-trip the same failure mode the plugin saw at boot"
    );

    server.cleanup().await;
}
