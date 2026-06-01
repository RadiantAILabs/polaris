//! In-process ring buffer and run/session query surface for the dashboard.
//!
//! [`SpanBuffer`] holds the most recent [`SpanRecord`]s emitted by
//! [`RecordingLayer`](super::capture::RecordingLayer) and exposes derived
//! views — distinct runs, distinct sessions, hierarchical span trees,
//! and token-usage rollups — that back the tracing HTTP endpoints.

use super::SpanKind;
use super::SpanRecord;
use super::capture::SpanRecordSink;
use parking_lot::Mutex;
use polaris_system::api::API;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Label key carrying the session identifier — propagated by
/// `SessionsAPI` via the `polaris.label.session_id` tracing field.
const SESSION_LABEL_KEY: &str = "session_id";
/// Label key carrying the agent type — propagated by `SessionsAPI` and
/// used as a friendly fallback in summaries.
const AGENT_TYPE_LABEL_KEY: &str = "agent_type";

/// Ring buffer of recent tracing records, with run/session/usage query
/// views for the dashboard.
///
/// Reach for this when a plugin needs to read recent graph-execution
/// history in-process — distinct runs, hierarchical span trees, or
/// token-usage rollups — without parsing log output or reaching into
/// `tracing` internals. The buffer is `Arc`-backed (a
/// `parking_lot::Mutex<VecDeque<_>>` inside) and cheaply cloneable; all
/// clones share the same backing ring.
///
/// # Provided by
///
/// [`TracingPlugin`](super::TracingPlugin), which calls
/// [`Server::insert_api`](polaris_system::server::Server::insert_api)
/// during `build()` when the `dashboard` feature is enabled. The buffer
/// does not exist without that feature.
///
/// # Surface
///
/// | Method | Description |
/// |--------|-------------|
/// | [`new`](Self::new) | Creates a buffer with the default capacity ([`DEFAULT_CAPACITY`](Self::DEFAULT_CAPACITY), 1024 records). |
/// | [`with_capacity`](Self::with_capacity) | Creates a buffer retaining a given number of records. |
/// | [`capacity`](Self::capacity) | Returns the maximum number of records retained. |
/// | [`push`](Self::push) | Appends one record, evicting the oldest on overflow. The write path used by the recording layer. |
/// | [`snapshot`](Self::snapshot) | Clones up to `limit` of the most recent records, oldest-to-newest. |
/// | [`distinct_runs`](Self::distinct_runs) | Up to `limit` distinct runs, most-recently-active first. |
/// | [`distinct_runs_by_label`](Self::distinct_runs_by_label) | Distinct runs filtered to those carrying a `key == value` label. |
/// | [`latest_run_for_label`](Self::latest_run_for_label) | The `run_id` of the most-recent run matching a `key == value` label. |
/// | [`distinct_sessions`](Self::distinct_sessions) | Up to `limit` distinct sessions, keyed by the `session_id` label. |
/// | [`run_tree`](Self::run_tree) | Hierarchical [`SpanTree`] for one run, with or without event payloads. |
/// | [`span`](Self::span) | One span's close record by `run_id` / `span_id`. |
/// | [`aggregate_usage`](Self::aggregate_usage) | Token-usage rollup across the whole buffer. |
/// | [`aggregate_usage_for_run`](Self::aggregate_usage_for_run) | Token-usage rollup for one run (`None` if absent). |
/// | [`aggregate_usage_by_label`](Self::aggregate_usage_by_label) | Token-usage rollup across records matching a `key == value` label. |
///
/// # Lifecycle
///
/// Available from the moment [`TracingPlugin`](super::TracingPlugin)'s
/// `build()` runs. Consumers resolve it via `server.api::<SpanBuffer>()`
/// during `build()` (if `TracingPlugin` was added first) or `ready()`.
/// [`push`](Self::push) is the runtime write path — driven by the
/// recording layer as spans close, and by
/// [`SpanStorePlugin`](super::SpanStorePlugin)'s `ready()` hydration. All
/// query methods are valid at any time; they observe whatever records are
/// in the ring at the moment of the call.
///
/// # Composition
///
/// **Provider-scoped.** Only [`TracingPlugin`](super::TracingPlugin)
/// inserts this API. Records flow in through the recording layer
/// `TracingPlugin` installs (and, optionally,
/// [`SpanStorePlugin`](super::SpanStorePlugin) hydration); consumers use
/// the query methods to read derived views.
///
/// # Example consumers
///
/// - `TracingPlugin`'s dashboard HTTP handlers — use the buffer as axum
///   handler state to serve the `/v1/tracing/*` and
///   `/v1/sessions/{id}/...` endpoints.
/// - [`SpanStorePlugin`](super::SpanStorePlugin) — calls
///   [`push`](Self::push) during `ready()` to hydrate the buffer from
///   durable storage so a resumed session's history survives a restart.
///
/// # Example
///
/// Provider side is automatic — adding [`TracingPlugin`](super::TracingPlugin)
/// with the `dashboard` feature inserts the buffer. A consumer plugin
/// resolves it during `ready()`:
///
/// ```no_run
/// use polaris_core_plugins::{ServerInfoPlugin, SpanBuffer, TracingPlugin};
/// use polaris_system::plugin::{Plugin, PluginId, Version};
/// use polaris_system::server::Server;
///
/// struct RunCountPlugin;
///
/// impl Plugin for RunCountPlugin {
///     const ID: &'static str = "example::run_count";
///     const VERSION: Version = Version::new(0, 0, 1);
///
///     fn build(&self, _server: &mut Server) {}
///
///     fn dependencies(&self) -> Vec<PluginId> {
///         vec![PluginId::of::<TracingPlugin>()]
///     }
///
///     async fn ready(&self, server: &mut Server) {
///         let buffer = server
///             .api::<SpanBuffer>()
///             .expect("TracingPlugin with `dashboard` feature provides SpanBuffer");
///         let runs = buffer.distinct_runs(50);
///         tracing::info!(count = runs.len(), "runs observed so far");
///     }
/// }
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(ServerInfoPlugin)
///     .add_plugins(polaris_app::AppPlugin::new(
///         polaris_app::AppConfig::new().with_host("127.0.0.1"),
///     ))
///     .add_plugins(polaris_models::ModelsPlugin)
///     .add_plugins(polaris_tools::ToolsPlugin)
///     // Provider: TracingPlugin inserts the SpanBuffer API.
///     .add_plugins(TracingPlugin::new())
///     .add_plugins(RunCountPlugin);
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct SpanBuffer {
    capacity: usize,
    records: Arc<Mutex<VecDeque<SpanRecord>>>,
    /// Set the first time the buffer evicts a record so the
    /// capacity-exceeded warning fires once, not per push. Shared across
    /// clones (all clones back the same ring, so they share the signal).
    overflow_warned: Arc<AtomicBool>,
}

impl API for SpanBuffer {}

/// Selects how much of a run [`SpanBuffer::run_tree`] materializes.
///
/// Deserializes from the lowercase variant names (`payloads`, `structure`)
/// so it can back the `?include=` query parameter directly — an unknown
/// value is rejected by the extractor rather than silently coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeView {
    /// Embed nested event records — and their fields and messages —
    /// inside each span node. The full payload view.
    Payloads,
    /// Return the tree shape and span metadata only, omitting event
    /// payloads. Cheaper when the frontend will lazy-load payloads on
    /// demand via [`SpanBuffer::span`].
    Structure,
}

impl SpanBuffer {
    /// Default number of records retained in the buffer.
    pub const DEFAULT_CAPACITY: usize = 1024;

    /// Creates a new buffer with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// Creates a new buffer with the provided capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            records: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            overflow_warned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns the maximum number of records retained.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Appends a tracing record, evicting the oldest record on overflow.
    pub fn push(&self, record: SpanRecord) {
        if self.capacity == 0 {
            return;
        }

        let evicted = {
            let mut guard = self.records.lock();
            let evicted = guard.len() >= self.capacity;
            if evicted {
                let _ = guard.pop_front();
            }
            guard.push_back(record);
            evicted
        };

        // Warn *after* releasing the lock. `RecordingLayer` captures tracing
        // events into this same buffer, so emitting while holding `records`
        // would recurse back into `push` on the same thread — relying on
        // `tracing-core`'s re-entrancy guard to avoid a deadlock on the
        // non-reentrant `parking_lot::Mutex`. Logging outside the lock keeps
        // the buffer correct on its own terms, independent of that guard.
        //
        // A ring buffer at steady state is always full, so eviction is the
        // normal case — warning per push would emit thousands of lines per
        // second. Warn once: enough for an operator to learn history is being
        // dropped before it can be queried, and to raise capacity if needed.
        if evicted && !self.overflow_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                capacity = self.capacity,
                "SpanBuffer reached capacity; evicting oldest records. Older \
                 history is dropped before it can be queried — raise capacity \
                 via TracingPlugin::with_span_buffer_capacity for more retention."
            );
        }
    }

    /// Clones up to `limit` of the most recent records in chronological order.
    #[must_use]
    pub fn snapshot(&self, limit: usize) -> Vec<SpanRecord> {
        let limit = limit.min(self.capacity);
        if limit == 0 {
            return Vec::new();
        }

        let guard = self.records.lock();
        let start = guard.len().saturating_sub(limit);
        guard.iter().skip(start).cloned().collect()
    }

    /// Returns up to `limit` distinct runs observed in the buffer, ordered by
    /// most-recently-active first.
    ///
    /// A run is identified by the `run_id` attached to a span/event record.
    /// Records without a `run_id` are skipped. `started_at` reports the
    /// earliest start observed for any span in the run, `duration_ms` and
    /// `outcome` are derived from the latest `polaris.graph.execute` close
    /// record matching the run.
    ///
    /// Runs are returned in descending order by their most-recent activity
    /// (`last_seen`), and capped at `limit`.
    #[must_use]
    pub fn distinct_runs(&self, limit: usize) -> Vec<RunSummary> {
        if limit == 0 {
            return Vec::new();
        }

        let guard = self.records.lock();
        let mut acc: HashMap<String, RunAccumulator> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for (idx, record) in guard.iter().enumerate() {
            let Some(run_id) = record.run_id.as_deref() else {
                continue;
            };
            let entry = acc.entry(run_id.to_owned()).or_insert_with(|| {
                order.push(run_id.to_owned());
                RunAccumulator::default()
            });
            entry.observe(record, idx);
        }
        drop(guard);

        // Sort by last-seen index descending — most recent runs first.
        let mut runs: Vec<(String, RunAccumulator)> = order
            .into_iter()
            .map(|id| {
                let accumulator = acc.remove(&id).expect("accumulator must exist");
                (id, accumulator)
            })
            .collect();
        runs.sort_by_key(|(_, accumulator)| std::cmp::Reverse(accumulator.last_seen_idx));

        runs.into_iter()
            .take(limit)
            .map(|(run_id, accumulator)| accumulator.into_summary(run_id))
            .collect()
    }

    /// Returns the hierarchical [`SpanTree`] for the given `run_id`, or
    /// `None` if the run is not present in the buffer.
    ///
    /// [`TreeView::Payloads`] embeds nested event records (and their
    /// fields/messages) inside each span node. [`TreeView::Structure`]
    /// returns the tree shape and span metadata only — useful when the
    /// frontend wants a cheap structural fetch and will load payloads
    /// lazily via [`SpanBuffer::span`].
    #[must_use]
    pub fn run_tree(&self, run_id: &str, view: TreeView) -> Option<SpanTree> {
        let guard = self.records.lock();
        let mut closes: Vec<SpanRecord> = Vec::new();
        let mut events: Vec<SpanRecord> = Vec::new();
        for record in guard.iter() {
            if record.run_id.as_deref() != Some(run_id) {
                continue;
            }
            match record.kind {
                SpanKind::SpanClose => closes.push(record.clone()),
                SpanKind::Event => events.push(record.clone()),
            }
        }
        drop(guard);

        if closes.is_empty() && events.is_empty() {
            return None;
        }

        let mut summary_acc = RunAccumulator::default();
        for (idx, record) in closes.iter().chain(events.iter()).enumerate() {
            summary_acc.observe(record, idx);
        }
        let summary = summary_acc.into_summary(run_id.to_owned());

        // Build node-by-span_id. `closes` is consumed here: each record's
        // owned fields move straight into its `SpanNode` instead of being
        // cloned a second time — they were already cloned once out of the
        // ring above.
        let mut nodes: HashMap<String, SpanNode> = HashMap::new();
        for record in closes {
            let Some(span_id) = record.span_id else {
                continue;
            };
            nodes.insert(
                span_id.clone(),
                SpanNode {
                    span_id,
                    parent_span_id: record.parent_span_id,
                    name: record.name,
                    level: record.level,
                    target: record.target,
                    started_at: record.started_at,
                    closed_at: Some(record.ts),
                    duration_ms: record.duration_ms,
                    fields: record.fields,
                    events: Vec::new(),
                    children: Vec::new(),
                },
            );
        }

        if view == TreeView::Payloads {
            for event in events {
                let parent_id = event.parent_span_id.clone();
                let span_event = SpanEvent {
                    ts: event.ts,
                    level: event.level,
                    target: event.target,
                    name: event.name,
                    message: event.message,
                    fields: event.fields,
                };
                if let Some(pid) = parent_id.as_deref()
                    && let Some(parent) = nodes.get_mut(pid)
                {
                    parent.events.push(span_event);
                } else {
                    // Orphan event without a known parent span — track on a
                    // synthetic "orphans" bucket on the tree.
                    // We collect these after assembly below.
                    nodes
                        .entry(String::new())
                        .or_insert_with(|| SpanNode {
                            span_id: String::new(),
                            parent_span_id: None,
                            name: "__orphan_events__".into(),
                            level: "info".into(),
                            target: "polaris.tracing".into(),
                            started_at: None,
                            closed_at: None,
                            duration_ms: None,
                            fields: Map::new(),
                            events: Vec::new(),
                            children: Vec::new(),
                        })
                        .events
                        .push(span_event);
                }
            }
        }

        // Resolve parent links. Drain the map into either a root, a child,
        // or an orphan list (parent referenced but never observed).
        let known_ids: HashSet<String> = nodes.keys().cloned().collect();
        let mut roots: Vec<SpanNode> = Vec::new();
        let mut orphans: Vec<SpanNode> = Vec::new();
        let mut by_parent: HashMap<String, Vec<SpanNode>> = HashMap::new();

        let orphan_event_bucket = nodes.remove("");

        for (_, node) in nodes.drain() {
            match node.parent_span_id.as_deref() {
                None => roots.push(node),
                Some(pid) => {
                    if known_ids.contains(pid) {
                        by_parent.entry(pid.to_owned()).or_default().push(node);
                    } else {
                        orphans.push(node);
                    }
                }
            }
        }

        // Recursively attach children.
        for root in &mut roots {
            attach_children(root, &mut by_parent);
        }
        for orphan in &mut orphans {
            attach_children(orphan, &mut by_parent);
        }

        // Any remaining entries in by_parent failed to attach (e.g., cycles
        // or partial buffers); flatten them into orphans as a safety net.
        let leftovers: Vec<Vec<SpanNode>> = by_parent.drain().map(|(_, v)| v).collect();
        for mut bucket in leftovers {
            for mut node in bucket.drain(..) {
                attach_children(&mut node, &mut by_parent);
                orphans.push(node);
            }
        }

        if let Some(bucket) = orphan_event_bucket {
            orphans.push(bucket);
        }

        // Stable order — by started_at then span_id.
        sort_nodes(&mut roots);
        sort_nodes(&mut orphans);

        Some(SpanTree {
            run_id: summary.run_id.clone(),
            agent_name: summary.agent_name.clone(),
            started_at: summary.started_at.clone(),
            duration_ms: summary.duration_ms,
            outcome: summary.outcome.clone(),
            labels: summary.labels.clone(),
            roots,
            orphans,
        })
    }

    /// Returns up to `limit` distinct runs whose accumulated labels contain
    /// the given `key == value` pair, most-recently-active first.
    ///
    /// Used by the per-session dashboard endpoints to fetch only the runs
    /// belonging to a particular session.
    #[must_use]
    pub fn distinct_runs_by_label(&self, key: &str, value: &str, limit: usize) -> Vec<RunSummary> {
        if limit == 0 {
            return Vec::new();
        }
        self.distinct_runs(usize::MAX)
            .into_iter()
            .filter(|run| run.labels.get(key).is_some_and(|v| v == value))
            .take(limit)
            .collect()
    }

    /// Returns the `run_id` of the most-recently-active run whose labels
    /// match the given `key == value` pair, or `None` if no run matches.
    #[must_use]
    pub fn latest_run_for_label(&self, key: &str, value: &str) -> Option<String> {
        self.distinct_runs_by_label(key, value, 1)
            .into_iter()
            .next()
            .map(|summary| summary.run_id)
    }

    /// Returns up to `limit` distinct sessions observed in the buffer,
    /// ordered by most-recently-active first.
    ///
    /// A session is identified by the `session_id` correlation label —
    /// the convention used by `SessionsAPI` to tag every span/event under
    /// a turn. Records without a `session_id` label are skipped. Each
    /// summary reports the distinct run count, the agent name carried in
    /// the `agent_type` label, the earliest span start observed, and the
    /// most-recent record timestamp.
    ///
    /// This surface intentionally decouples session discoverability from
    /// session-store membership. A session removed from the sessions
    /// store — for example by [`SessionsAPI::run_oneshot`], which deletes
    /// its ephemeral session as soon as the turn completes — still
    /// appears here as long as its records are in the buffer. Pair with
    /// [`SpanStorePlugin`](super::SpanStorePlugin) to extend that window
    /// across process restarts: the store hydrates the buffer on
    /// `ready()`, so historical sessions resurface alongside live ones.
    ///
    /// [`SessionsAPI::run_oneshot`]: https://docs.rs/polaris_sessions/latest/polaris_sessions/struct.SessionsAPI.html#method.run_oneshot
    #[must_use]
    pub fn distinct_sessions(&self, limit: usize) -> Vec<SessionSummary> {
        if limit == 0 {
            return Vec::new();
        }

        let guard = self.records.lock();
        let mut acc: HashMap<String, SessionAccumulator> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for (idx, record) in guard.iter().enumerate() {
            let Some(session_id) = record.labels.get(SESSION_LABEL_KEY) else {
                continue;
            };
            let entry = acc.entry(session_id.clone()).or_insert_with(|| {
                order.push(session_id.clone());
                SessionAccumulator::default()
            });
            entry.observe(record, idx);
        }
        drop(guard);

        let mut sessions: Vec<(String, SessionAccumulator)> = order
            .into_iter()
            .map(|id| {
                let accumulator = acc.remove(&id).expect("accumulator must exist");
                (id, accumulator)
            })
            .collect();
        sessions.sort_by_key(|(_, accumulator)| std::cmp::Reverse(accumulator.last_seen_idx));

        sessions
            .into_iter()
            .take(limit)
            .map(|(session_id, accumulator)| accumulator.into_summary(session_id))
            .collect()
    }

    /// Returns the close record for the given `run_id` / `span_id` pair, or
    /// `None` if the span has aged out of the ring.
    ///
    /// Used by the structure-only follow-up endpoint to lazily fetch a
    /// single span's event payloads.
    #[must_use]
    pub fn span(&self, run_id: &str, span_id: &str) -> Option<SpanRecord> {
        let guard = self.records.lock();
        for record in guard.iter().rev() {
            if record.kind == SpanKind::SpanClose
                && record.run_id.as_deref() == Some(run_id)
                && record.span_id.as_deref() == Some(span_id)
            {
                return Some(record.clone());
            }
        }
        None
    }

    /// Aggregates token usage across every record currently in the buffer.
    ///
    /// Records that do not carry `gen_ai.usage.*` attributes contribute
    /// nothing. When a [`UsagePricing`](super::UsagePricing) table is
    /// supplied, matching `(provider, model)` rates enrich `cost_usd` on
    /// the totals and breakdowns.
    #[must_use]
    pub fn aggregate_usage(
        &self,
        pricing: Option<&super::UsagePricing>,
    ) -> super::TokenUsageResponse {
        let guard = self.records.lock();
        super::usage::aggregate(guard.iter(), pricing)
    }

    /// Aggregates token usage for one run.
    ///
    /// Returns `None` when no records for `run_id` are present in the
    /// buffer, so HTTP handlers can 404 rather than return an all-zero
    /// response that's indistinguishable from "no LLM calls in this run".
    #[must_use]
    pub fn aggregate_usage_for_run(
        &self,
        run_id: &str,
        pricing: Option<&super::UsagePricing>,
    ) -> Option<super::TokenUsageResponse> {
        let guard = self.records.lock();
        let mut any = false;
        let filtered: Vec<&SpanRecord> = guard
            .iter()
            .filter(|record| {
                let matches = record.run_id.as_deref() == Some(run_id);
                if matches {
                    any = true;
                }
                matches
            })
            .collect();
        if !any {
            return None;
        }
        Some(super::usage::aggregate(filtered, pricing))
    }

    /// Aggregates token usage across every record whose labels contain the
    /// given `key == value` pair.
    ///
    /// Used by the session-scoped usage endpoint. Returns an empty
    /// response (zeroed totals, empty breakdowns) when no records match
    /// — sessions with no LLM activity are a valid steady state.
    #[must_use]
    pub fn aggregate_usage_by_label(
        &self,
        key: &str,
        value: &str,
        pricing: Option<&super::UsagePricing>,
    ) -> super::TokenUsageResponse {
        let guard = self.records.lock();
        let filtered = guard
            .iter()
            .filter(|record| record.labels.get(key).is_some_and(|v| v == value));
        super::usage::aggregate(filtered, pricing)
    }
}

impl Default for SpanBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanRecordSink for SpanBuffer {
    fn push(&self, record: SpanRecord) {
        SpanBuffer::push(self, record);
    }
}

fn attach_children(node: &mut SpanNode, by_parent: &mut HashMap<String, Vec<SpanNode>>) {
    if let Some(mut children) = by_parent.remove(&node.span_id) {
        for child in &mut children {
            attach_children(child, by_parent);
        }
        sort_nodes(&mut children);
        node.children = children;
    }
}

fn sort_nodes(nodes: &mut [SpanNode]) {
    nodes.sort_by(|a, b| {
        a.started_at
            .as_deref()
            .cmp(&b.started_at.as_deref())
            .then_with(|| a.span_id.cmp(&b.span_id))
    });
}

#[derive(Default, Debug)]
struct RunAccumulator {
    agent_name: Option<String>,
    earliest_start: Option<String>,
    latest_close: Option<String>,
    root_duration_ms: Option<u64>,
    outcome: Option<String>,
    last_seen_idx: usize,
    labels: BTreeMap<String, String>,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
}

impl RunAccumulator {
    fn observe(&mut self, record: &SpanRecord, idx: usize) {
        // Track ordering — last_seen drives the run summary sort.
        self.last_seen_idx = self.last_seen_idx.max(idx);

        // Merge correlation labels — first-write-wins. Skip the
        // `entry(key.clone())` path when the key is already present so
        // later spans in the same run don't pay a `String` clone per
        // label while the buffer mutex is held.
        for (key, value) in &record.labels {
            if !self.labels.contains_key(key) {
                self.labels.insert(key.clone(), value.clone());
            }
        }

        if self.agent_name.is_none() {
            self.agent_name = self.labels.get("agent_type").cloned().or_else(|| {
                record
                    .fields
                    .get("polaris.session.agent_type")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            });
        }
        if let Some(start) = record.started_at.as_deref() {
            self.earliest_start = Some(match &self.earliest_start {
                Some(existing) if existing.as_str() <= start => existing.clone(),
                _ => start.to_owned(),
            });
        }
        // Pin tree-wide outcome and duration on the root `polaris.graph.execute`
        // close record — every nested span also closes but its `duration_ms`
        // only reports its own scope.
        if record.kind == SpanKind::SpanClose && record.name == "polaris.graph.execute" {
            self.latest_close = Some(record.ts.clone());
            self.root_duration_ms = record.duration_ms.or(self.root_duration_ms);
            self.outcome = Some(if record.level == "error" {
                "error".to_owned()
            } else {
                "success".to_owned()
            });
        }

        // Roll up LLM token usage across every record in the run — the same
        // `gen_ai.usage.*` fields the buffer-wide usage aggregator reads, so
        // the run summary agrees with the `/v1/tracing/runs/{id}/usage`
        // endpoint. Records without usage fields contribute nothing.
        self.input_tokens = self.input_tokens.saturating_add(
            record
                .fields
                .get("gen_ai.usage.input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        self.output_tokens = self.output_tokens.saturating_add(
            record
                .fields
                .get("gen_ai.usage.output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        if let Some(cost) = record
            .fields
            .get("gen_ai.usage.cost_usd")
            .and_then(Value::as_f64)
        {
            self.cost_usd += cost;
        }
    }

    fn into_summary(self, run_id: String) -> RunSummary {
        RunSummary {
            run_id,
            agent_name: self.agent_name,
            started_at: self.earliest_start,
            duration_ms: self.root_duration_ms,
            outcome: self.outcome,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            labels: self.labels,
        }
    }
}

#[derive(Default, Debug)]
struct SessionAccumulator {
    agent_name: Option<String>,
    earliest_start: Option<String>,
    latest_ts: Option<String>,
    last_seen_idx: usize,
    runs: HashSet<String>,
}

impl SessionAccumulator {
    fn observe(&mut self, record: &SpanRecord, idx: usize) {
        self.last_seen_idx = self.last_seen_idx.max(idx);

        if self.agent_name.is_none() {
            self.agent_name = record.labels.get(AGENT_TYPE_LABEL_KEY).cloned();
        }

        if let Some(start) = record.started_at.as_deref() {
            self.earliest_start = Some(match &self.earliest_start {
                Some(existing) if existing.as_str() <= start => existing.clone(),
                _ => start.to_owned(),
            });
        }

        let ts = record.ts.as_str();
        if !ts.is_empty() {
            self.latest_ts = Some(match &self.latest_ts {
                Some(existing) if existing.as_str() >= ts => existing.clone(),
                _ => ts.to_owned(),
            });
        }

        if let Some(run_id) = record.run_id.clone() {
            self.runs.insert(run_id);
        }
    }

    fn into_summary(self, session_id: String) -> SessionSummary {
        SessionSummary {
            session_id,
            agent_name: self.agent_name,
            run_count: self.runs.len(),
            started_at: self.earliest_start,
            last_seen_at: self.latest_ts,
        }
    }
}

/// Summary view of one observed run, returned by
/// [`SpanBuffer::distinct_runs`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct RunSummary {
    /// Run identifier.
    pub run_id: String,
    /// Agent name, when one was attached to the run via a `RunLabels`
    /// `agent_type` entry (sessions plugin convention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// ISO-8601 timestamp of the earliest span start observed for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Wall-clock duration of the root `polaris.graph.execute` span, when
    /// observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    /// `"success"` / `"error"` / `None` — inferred from the root span's
    /// level at close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Sum of `gen_ai.usage.input_tokens` across every record observed for
    /// this run. `0` when the run made no token-reporting LLM calls.
    #[serde(default)]
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub input_tokens: u64,
    /// Sum of `gen_ai.usage.output_tokens` across every record observed for
    /// this run. `0` when the run made no token-reporting LLM calls.
    #[serde(default)]
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub output_tokens: u64,
    /// Sum of `gen_ai.usage.cost_usd` across every record observed for
    /// this run. `0.0` when no record carried a priced LLM call — providers
    /// without a pricing entry never record the field, so this stays zero.
    #[serde(default)]
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub cost_usd: f64,
    /// Correlation labels merged from every record observed for this run.
    ///
    /// Populated from the `polaris.label.<key>` tracing field convention.
    /// Conventional keys include `session_id`, `agent_type`, and `turn`,
    /// allowing the dashboard to join a run back to the owning session.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, string>"))]
    pub labels: BTreeMap<String, String>,
}

/// Summary view of one observed session, returned by
/// [`SpanBuffer::distinct_sessions`].
///
/// Pairs with the [`RunSummary`] / [`SpanTree`] surfaces — clicking a
/// session in the dashboard's tracing-sessions list populates
/// `session_id` for the existing `sessions-runs` / `sessions-run-tree`
/// chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct SessionSummary {
    /// Session identifier (the `session_id` correlation label).
    pub session_id: String,
    /// Agent name, when one was attached via the `agent_type` label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Number of distinct runs observed for the session.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub run_count: usize,
    /// ISO-8601 timestamp of the earliest span start observed for the
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// ISO-8601 timestamp of the most recent record observed for the
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
}

/// Hierarchical view of one run, returned by [`SpanBuffer::run_tree`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct SpanTree {
    /// Run identifier this tree describes.
    pub run_id: String,
    /// Agent name, when one was attached via run labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Earliest span start observed for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Root span duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    /// Inferred outcome of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Correlation labels merged from every record observed for this run.
    /// Mirrors [`RunSummary::labels`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, string>"))]
    pub labels: BTreeMap<String, String>,
    /// Top-level span nodes (no parent in the buffer).
    pub roots: Vec<SpanNode>,
    /// Spans whose declared parent is missing from the buffer (e.g., the
    /// parent aged out of the ring).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<SpanNode>,
}

/// One node in a [`SpanTree`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct SpanNode {
    /// Stable per-process span id.
    pub span_id: String,
    /// Parent span id when nested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    /// Span name (matches the `name!` argument from the macro).
    pub name: String,
    /// Span level (`info`, `warn`, ...).
    pub level: String,
    /// Tracing metadata target.
    pub target: String,
    /// ISO-8601 timestamp the span opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// ISO-8601 timestamp the span closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    /// Wall-clock duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    /// Structured fields captured on the span.
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, unknown>"))]
    pub fields: Map<String, Value>,
    /// Inline event records that fired inside this span. Empty when the
    /// tree was built with `include_payloads = false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpanEvent>,
    /// Child span nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SpanNode>,
}

/// One event payload nested inside a [`SpanNode`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct SpanEvent {
    /// ISO-8601 timestamp the event fired.
    pub ts: String,
    /// Event level.
    pub level: String,
    /// Tracing metadata target.
    pub target: String,
    /// Event name (typically derived from the source location).
    pub name: String,
    /// Optional message field extracted from the tracing payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Structured fields captured from the event.
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, unknown>"))]
    pub fields: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing_plugin::capture::timestamp_now;

    fn make_record(idx: usize) -> SpanRecord {
        SpanRecord {
            ts: idx.to_string(),
            started_at: None,
            duration_ms: None,
            level: "info".to_owned(),
            target: "tests".into(),
            name: format!("record-{idx}"),
            kind: SpanKind::Event,
            span_id: None,
            parent_span_id: None,
            run_id: None,
            labels: BTreeMap::new(),
            fields: Map::new(),
            message: None,
        }
    }

    #[test]
    fn snapshot_returns_recent_tail() {
        let buffer = SpanBuffer::with_capacity(3);
        for idx in 0..5 {
            buffer.push(make_record(idx));
        }

        let snapshot = buffer.snapshot(2);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].name, "record-3");
        assert_eq!(snapshot[1].name, "record-4");
    }

    #[test]
    fn zero_capacity_buffer_drops_pushes_silently() {
        let buffer = SpanBuffer::with_capacity(0);
        buffer.push(make_record(0));
        buffer.push(make_record(1));
        assert!(buffer.snapshot(usize::MAX).is_empty());
    }

    #[test]
    fn snapshot_with_zero_limit_is_empty() {
        let buffer = SpanBuffer::with_capacity(4);
        buffer.push(make_record(0));
        assert!(buffer.snapshot(0).is_empty());
    }

    fn record_with_run(run_id: &str, name: &str, kind: SpanKind) -> SpanRecord {
        SpanRecord {
            ts: timestamp_now(),
            started_at: Some(timestamp_now()),
            duration_ms: Some(5),
            level: "info".into(),
            target: "tests".into(),
            name: name.into(),
            kind,
            span_id: Some(format!("{name}-id")),
            parent_span_id: None,
            run_id: Some(run_id.into()),
            labels: BTreeMap::new(),
            fields: Map::new(),
            message: None,
        }
    }

    #[test]
    fn distinct_runs_groups_and_orders_recent_first() {
        let buffer = SpanBuffer::with_capacity(64);
        buffer.push(record_with_run(
            "run-1",
            "polaris.graph.execute",
            SpanKind::SpanClose,
        ));
        buffer.push(record_with_run(
            "run-2",
            "polaris.graph.execute",
            SpanKind::SpanClose,
        ));
        buffer.push(record_with_run("run-1", "event", SpanKind::Event));

        let runs = buffer.distinct_runs(10);
        assert_eq!(runs.len(), 2);
        // Most-recent-activity first — run-1 had the latest event.
        assert_eq!(runs[0].run_id, "run-1");
        assert_eq!(runs[1].run_id, "run-2");
        assert_eq!(runs[0].outcome.as_deref(), Some("success"));
    }

    #[test]
    fn distinct_runs_sums_token_usage_across_records() {
        let buffer = SpanBuffer::with_capacity(64);

        let mut first = record_with_run("run-tok", "polaris.model.chat", SpanKind::SpanClose);
        first
            .fields
            .insert("gen_ai.usage.input_tokens".into(), Value::from(120));
        first
            .fields
            .insert("gen_ai.usage.output_tokens".into(), Value::from(45));
        first
            .fields
            .insert("gen_ai.usage.cost_usd".into(), Value::from(0.0125));
        buffer.push(first);

        let mut second = record_with_run("run-tok", "polaris.model.chat", SpanKind::SpanClose);
        second
            .fields
            .insert("gen_ai.usage.input_tokens".into(), Value::from(80));
        second
            .fields
            .insert("gen_ai.usage.output_tokens".into(), Value::from(20));
        second
            .fields
            .insert("gen_ai.usage.cost_usd".into(), Value::from(0.005));
        buffer.push(second);

        // A record carrying no usage fields contributes nothing.
        buffer.push(record_with_run("run-tok", "event", SpanKind::Event));

        let runs = buffer.distinct_runs(10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].input_tokens, 200, "input tokens sum across spans");
        assert_eq!(runs[0].output_tokens, 65, "output tokens sum across spans");
        assert!(
            (runs[0].cost_usd - 0.0175).abs() < 1e-9,
            "cost_usd sums across spans: {}",
            runs[0].cost_usd
        );
    }

    #[test]
    fn distinct_runs_reports_zero_tokens_for_runs_without_usage() {
        let buffer = SpanBuffer::with_capacity(16);
        buffer.push(record_with_run(
            "run-1",
            "polaris.graph.execute",
            SpanKind::SpanClose,
        ));

        let runs = buffer.distinct_runs(10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].input_tokens, 0);
        assert_eq!(runs[0].output_tokens, 0);
        assert_eq!(runs[0].cost_usd, 0.0);
    }

    #[test]
    fn distinct_runs_skips_records_without_run_id() {
        let buffer = SpanBuffer::with_capacity(8);
        buffer.push(make_record(0));
        assert!(buffer.distinct_runs(10).is_empty());
    }

    #[test]
    fn run_tree_returns_none_for_unknown_run() {
        let buffer = SpanBuffer::with_capacity(8);
        assert!(buffer.run_tree("missing", TreeView::Payloads).is_none());
    }

    #[test]
    fn run_tree_nests_children_under_parent() {
        let buffer = SpanBuffer::with_capacity(32);
        let mut root = record_with_run("run-1", "polaris.graph.execute", SpanKind::SpanClose);
        root.span_id = Some("root".into());
        root.parent_span_id = None;
        let mut child =
            record_with_run("run-1", "polaris.graph.execute_system", SpanKind::SpanClose);
        child.span_id = Some("child".into());
        child.parent_span_id = Some("root".into());
        buffer.push(root);
        buffer.push(child);

        let tree = buffer
            .run_tree("run-1", TreeView::Payloads)
            .expect("tree present");
        assert_eq!(tree.roots.len(), 1);
        assert_eq!(tree.roots[0].span_id, "root");
        assert_eq!(tree.roots[0].children.len(), 1);
        assert_eq!(tree.roots[0].children[0].span_id, "child");
    }

    #[test]
    fn run_tree_drops_event_payloads_when_structure_only() {
        let buffer = SpanBuffer::with_capacity(32);
        let mut root = record_with_run("run-X", "polaris.graph.execute", SpanKind::SpanClose);
        root.span_id = Some("root".into());
        let mut event = record_with_run("run-X", "noisy_event", SpanKind::Event);
        event.parent_span_id = Some("root".into());
        event.span_id = None;
        event.message = Some("verbose payload".into());
        buffer.push(root);
        buffer.push(event);

        let with_payloads = buffer.run_tree("run-X", TreeView::Payloads).expect("tree");
        assert_eq!(with_payloads.roots[0].events.len(), 1);
        assert_eq!(
            with_payloads.roots[0].events[0].message.as_deref(),
            Some("verbose payload")
        );

        let structure_only = buffer.run_tree("run-X", TreeView::Structure).expect("tree");
        assert!(
            structure_only.roots[0].events.is_empty(),
            "structure-only must drop event payloads"
        );
    }

    #[test]
    fn span_returns_close_record_when_present() {
        let buffer = SpanBuffer::with_capacity(8);
        let mut close =
            record_with_run("run-Z", "polaris.graph.execute_system", SpanKind::SpanClose);
        close.span_id = Some("span-Z1".into());
        buffer.push(close);

        let found = buffer.span("run-Z", "span-Z1").expect("span present");
        assert_eq!(found.name, "polaris.graph.execute_system");
    }

    #[test]
    fn span_returns_none_when_run_or_span_missing() {
        let buffer = SpanBuffer::with_capacity(8);
        let mut close =
            record_with_run("run-Z", "polaris.graph.execute_system", SpanKind::SpanClose);
        close.span_id = Some("span-Z1".into());
        buffer.push(close);

        assert!(buffer.span("wrong-run", "span-Z1").is_none());
        assert!(buffer.span("run-Z", "wrong-span").is_none());
    }

    #[test]
    fn distinct_runs_by_label_filters_by_session() {
        let buffer = SpanBuffer::with_capacity(64);
        let mut a = record_with_run("run-a", "polaris.graph.execute", SpanKind::SpanClose);
        a.labels.insert("session_id".into(), "sess-1".into());
        let mut b = record_with_run("run-b", "polaris.graph.execute", SpanKind::SpanClose);
        b.labels.insert("session_id".into(), "sess-2".into());
        let mut c = record_with_run("run-c", "polaris.graph.execute", SpanKind::SpanClose);
        c.labels.insert("session_id".into(), "sess-1".into());
        buffer.push(a);
        buffer.push(b);
        buffer.push(c);

        let runs = buffer.distinct_runs_by_label("session_id", "sess-1", 10);
        let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["run-c", "run-a"],
            "filter to sess-1 runs in most-recent-first order"
        );

        let latest = buffer.latest_run_for_label("session_id", "sess-1");
        assert_eq!(latest.as_deref(), Some("run-c"));
        assert!(
            buffer
                .latest_run_for_label("session_id", "missing")
                .is_none()
        );
    }

    #[test]
    fn distinct_sessions_groups_records_by_session_label() {
        let buffer = SpanBuffer::with_capacity(64);
        let mut a = record_with_run("run-1", "polaris.graph.execute", SpanKind::SpanClose);
        a.labels.insert("session_id".into(), "sess-A".into());
        a.labels.insert("agent_type".into(), "react".into());
        let mut b = record_with_run("run-2", "polaris.graph.execute", SpanKind::SpanClose);
        b.labels.insert("session_id".into(), "sess-B".into());
        let mut c = record_with_run("run-3", "polaris.graph.execute", SpanKind::SpanClose);
        c.labels.insert("session_id".into(), "sess-A".into());

        buffer.push(a);
        buffer.push(b);
        buffer.push(c);

        let sessions = buffer.distinct_sessions(10);
        assert_eq!(sessions.len(), 2, "two distinct session_ids observed");
        // Most-recently-active first — the third push was for sess-A.
        assert_eq!(sessions[0].session_id, "sess-A");
        assert_eq!(sessions[0].run_count, 2, "two distinct runs for sess-A");
        assert_eq!(sessions[0].agent_name.as_deref(), Some("react"));
        assert_eq!(sessions[1].session_id, "sess-B");
        assert_eq!(sessions[1].run_count, 1);
    }

    #[test]
    fn distinct_sessions_skips_records_without_session_label() {
        let buffer = SpanBuffer::with_capacity(16);
        let rec = record_with_run("run-1", "evt", SpanKind::Event);
        buffer.push(rec);
        assert!(
            buffer.distinct_sessions(10).is_empty(),
            "records without a session_id label do not surface as sessions"
        );
    }

    #[test]
    fn distinct_sessions_zero_limit_returns_empty() {
        let buffer = SpanBuffer::with_capacity(8);
        let mut rec = record_with_run("run-1", "evt", SpanKind::Event);
        rec.labels.insert("session_id".into(), "sess-Z".into());
        buffer.push(rec);
        assert!(buffer.distinct_sessions(0).is_empty());
    }

    #[test]
    fn run_summary_labels_drive_agent_name_fallback() {
        let buffer = SpanBuffer::with_capacity(16);
        let mut rec = record_with_run("run-l", "polaris.graph.execute", SpanKind::SpanClose);
        rec.labels.insert("agent_type".into(), "react".into());
        buffer.push(rec);

        let runs = buffer.distinct_runs(10);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].agent_name.as_deref(),
            Some("react"),
            "agent_type label drives agent_name in summary"
        );
        assert_eq!(
            runs[0].labels.get("agent_type").map(String::as_str),
            Some("react"),
        );
    }

    #[test]
    fn run_tree_handles_aged_out_root_via_orphan_bucket() {
        let buffer = SpanBuffer::with_capacity(8);
        let mut child = record_with_run(
            "run-orphan",
            "polaris.graph.execute_system",
            SpanKind::SpanClose,
        );
        child.span_id = Some("c1".into());
        child.parent_span_id = Some("aged-out-root".into());
        buffer.push(child);

        let tree = buffer
            .run_tree("run-orphan", TreeView::Payloads)
            .expect("tree");
        assert!(tree.roots.is_empty(), "no root in buffer");
        assert_eq!(tree.orphans.len(), 1, "child must surface in orphans");
        assert_eq!(tree.orphans[0].span_id, "c1");
    }
}
