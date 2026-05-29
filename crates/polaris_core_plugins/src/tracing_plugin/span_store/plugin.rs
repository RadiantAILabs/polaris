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
use parking_lot::Mutex;
use polaris_system::api::API;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Correlation label key carrying the session identifier. Mirrors the
/// constant used by the dashboard.
const SESSION_LABEL_KEY: &str = "session_id";

/// Bound on the writer's pending-record queue. Past this, the tracing hot
/// path drops records rather than blocking (see [`StoreSink::push`]).
const WRITER_QUEUE_CAPACITY: usize = 8192;

/// Upper bound on records pulled from the queue per store round-trip.
/// Coalescing lets durable backends amortize their per-write barrier (an
/// `fsync` for [`FileSpanStore`](crate::FileSpanStore)) across the batch.
const WRITER_BATCH_SIZE: usize = 256;

/// How long [`SpanStorePlugin::cleanup`] waits for the writer to drain its
/// queue and stop. Matches `AppPlugin`'s graceful-shutdown budget.
const WRITER_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Message handed to the background writer task.
///
/// The record is boxed so the rare `Drain` variant does not bloat every
/// queue slot to the size of a full [`SpanRecord`].
enum WriterCommand {
    /// Persist `record` under `session_id`.
    Write {
        /// Session the record belongs to.
        session_id: String,
        /// The record to persist.
        record: Box<SpanRecord>,
    },
    /// Drain everything already queued, then stop. Sent by `cleanup()`.
    Drain,
}

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
///    and tracing event is enqueued — not written inline — and a single
///    background writer task drains the queue into the configured backend
///    keyed by its `session_id` label. Routing every write through one
///    task gives the design three properties the tracing hot path needs:
///    bounded memory (the queue drops, with a rate-limited warning, rather
///    than spawning an unbounded number of tasks under burst), a durability
///    barrier paid once per drained batch rather than once per record, and
///    a drain on shutdown ([`cleanup`](#lifecycle)) so records emitted in
///    the final moments before exit are still persisted.
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
/// - **`ready()`** — spawns the single background writer task that drains
///   the record queue into the store (independent of the `dashboard`
///   feature). When the `dashboard` feature is also enabled, replays
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
/// - **`cleanup()`** — signals the writer to drain its queue, then awaits
///   it (up to a five-second grace) so records still in flight at shutdown
///   reach the store before the process exits.
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
    /// Sender handed to the [`StoreSink`] (and used by `cleanup()` to send
    /// [`WriterCommand::Drain`]). The channel is created at construction so
    /// the sink wired up in `build()` and the task spawned in `ready()`
    /// share one queue.
    command_tx: mpsc::Sender<WriterCommand>,
    /// Receiver consumed by the writer task. Taken out in `ready()`; the
    /// `Mutex<Option<_>>` exists only because the [`Plugin`] lifecycle
    /// methods take `&self`.
    command_rx: Mutex<Option<mpsc::Receiver<WriterCommand>>>,
    /// Join handle for the spawned writer, awaited in `cleanup()`.
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl SpanStorePlugin {
    /// Creates a new plugin backed by the given store.
    #[must_use]
    pub fn new(store: DynSpanStore) -> Self {
        let (command_tx, command_rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        Self {
            store,
            command_tx,
            command_rx: Mutex::new(Some(command_rx)),
            writer: Mutex::new(None),
        }
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
        let sink: Arc<dyn SpanRecordSink> = Arc::new(StoreSink::new(self.command_tx.clone()));
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
        // Spawn the single writer task that drains the record queue into
        // the store. This is the durability path and runs regardless of the
        // `dashboard` feature — hydration below is the dashboard-only extra.
        if let Some(rx) = self.command_rx.lock().take() {
            let store = self.store.clone();
            *self.writer.lock() = Some(tokio::spawn(run_writer(store, rx)));
        }

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

    async fn cleanup(&self, _server: &mut Server) {
        let Some(handle) = self.writer.lock().take() else {
            return;
        };

        // Ask the writer to drain what is queued and stop, then await it.
        // Bound the whole drain by the grace window: a wedged store append
        // must not hang shutdown. `send().await` (not `try_send`) so the
        // signal is delivered even when the queue is momentarily full — the
        // writer frees capacity every batch, so it lands promptly.
        let command_tx = self.command_tx.clone();
        let drain = async move {
            let _ = command_tx.send(WriterCommand::Drain).await;
            let _ = handle.await;
        };

        if tokio::time::timeout(WRITER_DRAIN_GRACE, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                grace_secs = WRITER_DRAIN_GRACE.as_secs(),
                "SpanStorePlugin: writer drain timed out; some records may not be persisted"
            );
        }
    }

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<TracingPlugin>()]
    }
}

/// Drains [`WriterCommand`]s into the store until the channel closes or a
/// [`WriterCommand::Drain`] arrives.
///
/// Records are pulled in batches ([`WRITER_BATCH_SIZE`]) and handed to
/// [`SpanStore::append_batch`], so a durable backend pays its write barrier
/// once per batch rather than once per record. On `Drain`, everything still
/// queued is pulled with non-blocking `try_recv` and flushed in a final
/// batch before the task returns — this is what makes shutdown lossless.
async fn run_writer(store: DynSpanStore, mut rx: mpsc::Receiver<WriterCommand>) {
    let mut batch = Vec::with_capacity(WRITER_BATCH_SIZE);
    loop {
        batch.clear();
        if rx.recv_many(&mut batch, WRITER_BATCH_SIZE).await == 0 {
            // All senders dropped: nothing more will ever arrive.
            break;
        }

        let mut writes = Vec::with_capacity(batch.len());
        let mut draining = false;
        for command in batch.drain(..) {
            match command {
                WriterCommand::Write { session_id, record } => {
                    writes.push((session_id, *record));
                }
                WriterCommand::Drain => draining = true,
            }
        }

        if draining {
            // Sweep up anything already enqueued behind the Drain marker
            // without awaiting (and thus without racing late arrivals).
            while let Ok(command) = rx.try_recv() {
                if let WriterCommand::Write { session_id, record } = command {
                    writes.push((session_id, *record));
                }
            }
        }

        if !writes.is_empty()
            && let Err(err) = store.append_batch(&writes).await
        {
            tracing::warn!(
                error = %err,
                count = writes.len(),
                "SpanStorePlugin: batch append failed"
            );
        }

        if draining {
            break;
        }
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

/// [`SpanRecordSink`] adapter that enqueues records carrying a `session_id`
/// label onto the writer task's bounded queue.
///
/// Tracing layers run on whatever thread emitted the event and must not
/// block, so `push` is a non-blocking `try_send`: if the queue is full it
/// drops the record rather than stalling the emitting thread. Blocking the
/// hot path to apply backpressure would be worse than dropping a trace
/// record — so the design absorbs bursts up to the queue bound and sheds
/// load past it, surfacing the loss through a rate-limited warning and a
/// running drop count.
pub(super) struct StoreSink {
    tx: mpsc::Sender<WriterCommand>,
    dropped: AtomicU64,
}

impl StoreSink {
    fn new(tx: mpsc::Sender<WriterCommand>) -> Self {
        Self {
            tx,
            dropped: AtomicU64::new(0),
        }
    }
}

impl SpanRecordSink for StoreSink {
    fn push(&self, record: SpanRecord) {
        let Some(session_id) = record.labels.get(SESSION_LABEL_KEY).cloned() else {
            return;
        };

        if self
            .tx
            .try_send(WriterCommand::Write {
                session_id,
                record: Box::new(record),
            })
            .is_err()
        {
            // `Full` (back-pressured) or `Closed` (writer gone, e.g. after
            // shutdown). Either way the record is dropped. Warn on the
            // first drop and then at exponentially sparser intervals so a
            // sustained overload does not itself flood the logs.
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                tracing::warn!(
                    dropped,
                    "SpanStorePlugin: writer queue full or closed; dropping span record"
                );
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

    /// Drains every [`WriterCommand`] currently queued and returns the
    /// session-scoped writes, mirroring what the writer task sees.
    fn drain_writes(rx: &mut mpsc::Receiver<WriterCommand>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Ok(command) = rx.try_recv() {
            if let WriterCommand::Write { session_id, record } = command {
                out.push((session_id, record.name));
            }
        }
        out
    }

    #[tokio::test]
    async fn store_sink_enqueues_only_session_scoped_records() {
        let (tx, mut rx) = mpsc::channel(8);
        let sink = StoreSink::new(tx);

        sink.push(make(None, "unscoped"));
        sink.push(make(Some("sess-A"), "first"));
        sink.push(make(Some("sess-A"), "second"));

        // The unscoped record must never reach the queue — it could not be
        // queried per-session anyway.
        assert_eq!(
            drain_writes(&mut rx),
            vec![
                ("sess-A".to_string(), "first".to_string()),
                ("sess-A".to_string(), "second".to_string()),
            ],
        );
    }

    #[tokio::test]
    async fn store_sink_drops_records_past_queue_capacity() {
        // Capacity-1 queue: the first record fits, the rest are shed
        // instead of growing memory without bound.
        let (tx, mut rx) = mpsc::channel(1);
        let sink = StoreSink::new(tx);
        for i in 0..5 {
            sink.push(make(Some("sess-A"), &format!("rec-{i}")));
        }

        let queued = drain_writes(&mut rx);
        assert_eq!(queued.len(), 1, "only the queued record survives");
        assert_eq!(queued[0].1, "rec-0");
    }

    #[tokio::test]
    async fn writer_drains_queued_records_to_store_in_order() {
        let store: DynSpanStore = Arc::new(InMemorySpanStore::new());
        let (tx, rx) = mpsc::channel(8);
        let writer = tokio::spawn(run_writer(store.clone(), rx));

        for name in ["first", "second", "third"] {
            tx.send(WriterCommand::Write {
                session_id: "sess-A".to_string(),
                record: Box::new(make(Some("sess-A"), name)),
            })
            .await
            .unwrap();
        }

        // Drain signal + join is exactly the cleanup() path.
        tx.send(WriterCommand::Drain).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("writer must finish draining within the timeout")
            .expect("writer task must not panic");

        let names: Vec<_> = store
            .load("sess-A")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn writer_drain_flushes_records_queued_before_shutdown() {
        // The shutdown-loss regression: records enqueued just before the
        // Drain marker must still be persisted, not torn down with the task.
        let store: DynSpanStore = Arc::new(InMemorySpanStore::new());
        let (tx, rx) = mpsc::channel(64);

        // Queue work, then the Drain marker, all before the writer runs —
        // so the writer's very first batch contains records *and* the
        // marker. None may be lost.
        for i in 0..10 {
            tx.send(WriterCommand::Write {
                session_id: "sess-A".to_string(),
                record: Box::new(make(Some("sess-A"), &format!("rec-{i}"))),
            })
            .await
            .unwrap();
        }
        tx.send(WriterCommand::Drain).await.unwrap();

        let writer = tokio::spawn(run_writer(store.clone(), rx));
        tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .expect("writer must drain and stop")
            .expect("writer task must not panic");

        assert_eq!(
            store.load("sess-A").await.unwrap().len(),
            10,
            "every record queued before shutdown must be persisted"
        );
    }
}
