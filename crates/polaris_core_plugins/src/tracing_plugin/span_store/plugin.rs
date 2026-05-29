//! [`SpanStorePlugin`] — durable span/run history backend.

use super::DynSpanStore;
// Brought into scope for rustdoc intra-doc links.
#[expect(
    unused_imports,
    reason = "doc-only: keeps [`SpanStore`] intra-doc links resolvable"
)]
use super::SpanStore;
#[cfg(feature = "dashboard")]
use crate::tracing_plugin::SpanBuffer;
use crate::tracing_plugin::{
    RecordingLayer, SpanRecord, SpanRecordSink, TracingLayers, TracingPlugin,
};
use polaris_system::api::API;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::sync::Arc;

/// Correlation label key carrying the session identifier. Mirrors the
/// constant used by the dashboard.
const SESSION_LABEL_KEY: &str = "session_id";

/// Persists span/run history beyond the lifetime of one process.
///
/// Reach for this plugin when an operator must see a resumed session's
/// run history after a server restart — not just its identity.
///
/// The dashboard's [`SpanBuffer`] is a fixed-size in-memory ring — it
/// vanishes on restart. `polaris_sessions::SessionStore` keeps session
/// identity alive, but until this plugin is installed there is no
/// parallel store for the runs that produced that session's state.
/// The result is an operator-visible contradiction: the session store
/// resumes a session, the dashboard lists it, but the run-tree panel
/// returns `0` runs because the buffer that held them was lost on restart.
///
/// `SpanStorePlugin` closes the gap by:
///
/// 1. Installing a [`RecordingLayer`] backed by a [`SpanStore`]-routing
///    sink, alongside the dashboard's own buffer layer. Each closed span
///    and tracing event is appended to the configured backend keyed by its
///    `session_id` label.
/// 2. On `ready()`, replaying stored records into the [`SpanBuffer`]
///    (when present) in chronological order, up to the buffer's capacity,
///    so dashboard queries against a resumed session return non-empty
///    immediately after boot. When the store holds more records than the
///    buffer can retain, the oldest are evicted during replay — the
///    newest records deterministically win, independent of the backend's
///    session iteration order.
///
/// The plugin coexists with `OpenTelemetryPlugin` (under feature `otel`)
/// — both plugins push independent layers into [`TracingLayers`], and
/// neither knows about the other. It also runs without the `dashboard`
/// feature: in that mode the store is still populated, but there is no
/// in-memory buffer to hydrate. Operators can query the store directly
/// via the [`SpanStore`] trait.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | The store is held internally by the plugin; access via [`SpanStore`] in the configured backend. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | [`SpanStoreHandle`] | Trait-object handle (`Arc<dyn SpanStore>`) for downstream plugins that want to query stored history. |
///
/// # Dependencies
///
/// - [`TracingPlugin`] — owns the subscriber.
///
/// # Lifecycle
///
/// - **`build()`** — installs a [`RecordingLayer`] backed by a
///   [`SpanStore`]-routing sink into [`TracingLayers`], and inserts
///   the [`SpanStoreHandle`] API.
/// - **`ready()`** — when the `dashboard` feature is enabled, replays
///   stored records into the dashboard's [`SpanBuffer`] in chronological
///   order (by [`SpanRecord::ts`]), up to the buffer's capacity, so
///   queries against a resumed session return non-empty immediately after
///   boot. If the store holds more records than the buffer's capacity, the
///   oldest are evicted during replay so the newest records survive —
///   deterministically, regardless of session iteration order.
///   Without the `dashboard` feature there is no buffer to hydrate, so
///   `ready()` early-returns: the store is still populated, just nothing
///   to replay into. Store errors during hydration (`list_sessions` /
///   `load`) are logged via `tracing::warn!` and skipped, never panic.
/// - Registers no tick schedules.
///
/// # Extends
///
/// - [`TracingPlugin`] — pushes a [`RecordingLayer`] into
///   [`TracingLayers`] so closed spans and events are appended to the
///   configured [`SpanStore`] keyed by their `session_id` label.
/// - [`TracingPlugin`]'s dashboard [`SpanBuffer`] *(feature `dashboard`)* —
///   hydrates the in-memory buffer from the store on `ready()` so a
///   resumed session's run history survives a process restart.
///
/// # Example
///
/// With the `file-store` feature, back the plugin with a durable
/// [`FileSpanStore`](crate::FileSpanStore):
///
/// ```no_run
/// # #[cfg(feature = "file-store")]
/// # {
/// use std::sync::Arc;
/// use polaris_core_plugins::{
///     FileSpanStore, ServerInfoPlugin, SpanStorePlugin, TracingPlugin,
/// };
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let store = Arc::new(FileSpanStore::new("data/spans"));
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(SpanStorePlugin::new(store));
/// server.run().await;
/// # }
/// # }
/// ```
///
/// Any [`SpanStore`] implementation works — for example the always-available
/// [`InMemorySpanStore`](crate::InMemorySpanStore):
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris_core_plugins::{
///     InMemorySpanStore, ServerInfoPlugin, SpanStorePlugin, TracingPlugin,
/// };
/// use polaris_system::server::Server;
///
/// # async fn run() {
/// let store = Arc::new(InMemorySpanStore::new());
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(SpanStorePlugin::new(store));
/// server.run().await;
/// # }
/// ```
pub struct SpanStorePlugin {
    store: DynSpanStore,
}

impl SpanStorePlugin {
    /// Creates a new plugin backed by the given store.
    #[must_use]
    pub fn new(store: DynSpanStore) -> Self {
        Self { store }
    }
}

impl std::fmt::Debug for SpanStorePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The store is a `dyn SpanStore` trait object with no `Debug` bound.
        f.debug_struct("SpanStorePlugin").finish_non_exhaustive()
    }
}

impl Plugin for SpanStorePlugin {
    const ID: &'static str = "polaris::tracing::span_store";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let sink: Arc<dyn SpanRecordSink> = Arc::new(StoreSink::new(self.store.clone()));
        match server.get_resource_mut::<TracingLayers>() {
            Some(mut layers) => layers.push(RecordingLayer::with_sink(sink)),
            None => {
                // The framework enforces declared `dependencies()` so this
                // branch is unreachable when `TracingPlugin` is present.
                // The `SpanStoreHandle` API is still installed so consumers
                // see a clear "no recording layer" failure mode rather than
                // a process panic.
                tracing::error!(
                    "SpanStorePlugin: TracingLayers resource missing — \
                     TracingPlugin must be registered. Span recording is disabled."
                );
            }
        }

        server.insert_api(SpanStoreHandle(self.store.clone()));
    }

    async fn ready(&self, _server: &mut Server) {
        #[cfg(feature = "dashboard")]
        {
            let Some(buffer) = _server.api::<SpanBuffer>().map(|api| (*api).clone()) else {
                // Running without a dashboard buffer is a valid composition —
                // the store is still being populated, just nothing to hydrate.
                return;
            };

            let store = self.store.clone();
            let sessions = match store.list_sessions().await {
                Ok(sessions) => sessions,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "SpanStorePlugin: failed to list sessions during hydration"
                    );
                    return;
                }
            };

            // Collect every record across all sessions before replaying.
            // `list_sessions()` order is backend-defined and unstable
            // (`FileSpanStore` follows `read_dir`; `InMemorySpanStore`
            // follows `HashMap` iteration), so pushing per-session would
            // make eviction order — and thus which sessions survive a
            // capacity overflow — nondeterministic across restarts.
            let mut records = Vec::new();
            for session_id in sessions {
                match store.load(&session_id).await {
                    Ok(loaded) => records.extend(loaded),
                    Err(err) => {
                        tracing::warn!(
                            session_id = %session_id,
                            error = %err,
                            "SpanStorePlugin: failed to load session during hydration",
                        );
                    }
                }
            }

            let hydrated = replay_into_buffer(&buffer, records);

            tracing::info!(
                records = hydrated,
                "SpanStorePlugin: hydrated SpanBuffer from store"
            );
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<TracingPlugin>()]
    }
}

/// Replays stored records into the dashboard buffer in chronological
/// order, returning the number pushed.
///
/// Sorting by [`SpanRecord::ts`] (ISO-8601 UTC, lexicographically
/// ordered) makes replay independent of the backend's session iteration
/// order. The buffer is a fixed-size ring, so when the record count
/// exceeds its capacity the oldest are evicted and the newest
/// deterministically survive.
#[cfg(feature = "dashboard")]
fn replay_into_buffer(buffer: &SpanBuffer, mut records: Vec<SpanRecord>) -> usize {
    records.sort_by(|a, b| a.ts.cmp(&b.ts));
    let hydrated = records.len();
    for record in records {
        buffer.push(record);
    }
    hydrated
}

/// Build-time handle to the configured [`SpanStore`].
///
/// Reach for this when a plugin needs to query durable span/run history —
/// for example, to build a reporting surface, export stored runs, or
/// pre-warm a cache — without owning the store wiring itself.
/// [`SpanStorePlugin`] installs the store and the recording layer; this
/// handle is the read-side entry point for everyone else.
///
/// # Provided by
///
/// [`SpanStorePlugin`], which calls [`Server::insert_api`] during its
/// `build()` phase. No plugin registers it by default — it exists only
/// when `SpanStorePlugin` is added to the server.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`store`](Self::store) | Returns the underlying `Arc<dyn SpanStore>`. Call [`SpanStore`] methods (`append`, `load`, `list_sessions`) on it to read or write durable history. |
///
/// # Lifecycle
///
/// Available from the moment [`SpanStorePlugin::build`] runs. Consumers
/// may resolve it during their own `build()` (if `SpanStorePlugin` was
/// added first) or `ready()`. [`store`](Self::store) itself is callable
/// at any time, including at runtime — the returned `Arc` outlives the
/// build phase. Resolving the handle before `SpanStorePlugin` is built
/// yields `None`.
///
/// # Composition
///
/// **Provider-scoped.** Only [`SpanStorePlugin`] inserts this API.
/// Consumers obtain the handle and query the store through the
/// [`SpanStore`] trait; they do not contribute to the handle itself.
///
/// # Example consumers
///
/// No plugin in this workspace consumes `SpanStoreHandle` yet — the
/// dashboard buffer is hydrated by `SpanStorePlugin`'s own `ready()`,
/// not through this handle. It is a downstream extension point: any
/// plugin that depends on `SpanStorePlugin` can resolve the handle to
/// query stored history.
///
/// # Example
///
/// Provider side is automatic — adding [`SpanStorePlugin`] inserts the
/// handle. A consumer plugin resolves it during `ready()`:
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris_core_plugins::{InMemorySpanStore, SpanStoreHandle, SpanStorePlugin, TracingPlugin};
/// use polaris_core_plugins::ServerInfoPlugin;
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// struct HistoryReportPlugin;
///
/// impl Plugin for HistoryReportPlugin {
///     const ID: &'static str = "example::history_report";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn build(&self, _server: &mut Server) {}
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<SpanStorePlugin>()]
///     }
///
///     async fn ready(&self, server: &mut Server) {
///         let handle = server
///             .api::<SpanStoreHandle>()
///             .expect("SpanStorePlugin must be added before HistoryReportPlugin");
///         let sessions = handle.store().list_sessions().await.unwrap_or_default();
///         tracing::info!(count = sessions.len(), "stored sessions");
///     }
/// }
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(SpanStorePlugin::new(Arc::new(InMemorySpanStore::new())))
///     .add_plugins(HistoryReportPlugin);
/// server.run().await;
/// # }
/// ```
#[derive(Clone)]
pub struct SpanStoreHandle(DynSpanStore);

impl SpanStoreHandle {
    /// Returns the underlying store handle.
    #[must_use]
    pub fn store(&self) -> &DynSpanStore {
        &self.0
    }
}

impl std::fmt::Debug for SpanStoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Wraps a `dyn SpanStore` trait object with no `Debug` bound.
        f.debug_tuple("SpanStoreHandle").finish_non_exhaustive()
    }
}

impl API for SpanStoreHandle {}

/// [`SpanRecordSink`] adapter that forwards records carrying a
/// `session_id` label into a [`SpanStore`] via a fire-and-forget tokio
/// task.
///
/// Tracing layers run on whatever thread emitted the event; they must
/// not block. We avoid blocking I/O by dispatching the async store
/// `append` onto the current tokio runtime. When no runtime is available
/// (e.g. a synchronous test) the record is dropped — once per process
/// lifetime, a warning is emitted so operators can spot a misconfigured
/// runtime. The in-memory ring still receives the record and tests can
/// call the store directly when they need persistence.
pub(super) struct StoreSink {
    store: DynSpanStore,
    runtime_warned: std::sync::atomic::AtomicBool,
}

impl StoreSink {
    fn new(store: DynSpanStore) -> Self {
        Self {
            store,
            runtime_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl SpanRecordSink for StoreSink {
    fn push(&self, record: SpanRecord) {
        let Some(session_id) = record.labels.get(SESSION_LABEL_KEY).cloned() else {
            return;
        };
        let store = self.store.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(err) = store.append(&session_id, &record).await {
                        tracing::warn!(error = %err, "SpanStorePlugin: append failed");
                    }
                });
            }
            Err(_) => {
                if !self
                    .runtime_warned
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    tracing::warn!(
                        "SpanStorePlugin: no tokio runtime available; span records will not be persisted. \
                         Run the server inside `#[tokio::main]` or a `tokio::runtime::Runtime`."
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_plugin::span_store::InMemorySpanStore;
    use crate::tracing_plugin::{SpanKind, SpanRecord};
    use serde_json::Map;
    use std::collections::BTreeMap;

    fn make(session: Option<&str>, name: &str) -> SpanRecord {
        let mut labels = BTreeMap::new();
        if let Some(sid) = session {
            labels.insert("session_id".into(), sid.into());
        }
        SpanRecord {
            ts: "2026-05-17T00:00:00.000Z".into(),
            started_at: None,
            duration_ms: None,
            level: "info".into(),
            target: "tests".into(),
            name: name.into(),
            kind: SpanKind::Event,
            span_id: None,
            parent_span_id: None,
            run_id: Some("run-x".into()),
            labels,
            fields: Map::new(),
            message: None,
        }
    }

    #[cfg(feature = "dashboard")]
    fn make_ts(ts: &str, name: &str) -> SpanRecord {
        let mut record = make(Some("sess"), name);
        record.ts = ts.into();
        record
    }

    #[cfg(feature = "dashboard")]
    #[test]
    fn replay_orders_records_chronologically_regardless_of_input_order() {
        let buffer = SpanBuffer::with_capacity(8);
        // Deliberately out of order, as cross-session loads would arrive.
        let records = vec![
            make_ts("2026-05-17T00:00:03.000Z", "third"),
            make_ts("2026-05-17T00:00:01.000Z", "first"),
            make_ts("2026-05-17T00:00:02.000Z", "second"),
        ];

        assert_eq!(replay_into_buffer(&buffer, records), 3);

        let names: Vec<_> = buffer.snapshot(8).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[cfg(feature = "dashboard")]
    #[test]
    fn replay_keeps_newest_records_when_capacity_overflows() {
        let buffer = SpanBuffer::with_capacity(2);
        // Three records, smallest capacity: the two newest must survive,
        // independent of the order they were collected in.
        let records = vec![
            make_ts("2026-05-17T00:00:02.000Z", "middle"),
            make_ts("2026-05-17T00:00:03.000Z", "newest"),
            make_ts("2026-05-17T00:00:01.000Z", "oldest"),
        ];

        assert_eq!(replay_into_buffer(&buffer, records), 3);

        let names: Vec<_> = buffer.snapshot(8).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["middle", "newest"]);
    }

    #[tokio::test]
    async fn store_sink_drops_records_without_session_id() {
        let store: DynSpanStore = Arc::new(InMemorySpanStore::new());
        let sink = StoreSink::new(store.clone());
        sink.push(make(None, "unscoped"));
        // Give the spawned task a chance to run; there shouldn't be one.
        tokio::task::yield_now().await;
        assert!(store.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn store_sink_persists_records_with_session_id() {
        let store: DynSpanStore = Arc::new(InMemorySpanStore::new());
        let sink = StoreSink::new(store.clone());
        sink.push(make(Some("sess-A"), "first"));
        sink.push(make(Some("sess-A"), "second"));

        // The sink dispatches via tokio::spawn; poll on a real timer so
        // a slow runner produces a clear "took too long" failure rather
        // than an exhausted-yield-budget false negative.
        let records = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let records = store.load("sess-A").await.unwrap();
                if records.len() == 2 {
                    return records;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("spawned StoreSink appends must drain within the timeout window");
        assert_eq!(records[0].name, "first");
        assert_eq!(records[1].name, "second");
    }
}
