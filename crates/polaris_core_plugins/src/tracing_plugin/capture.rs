//! Subscriber-side capture pipeline: [`RecordingLayer`], [`SpanRecordSink`],
//! and the field visitor that produces [`SpanRecord`]s.
//!
//! The layer is sink-generic — the same recording machinery powers both the
//! dashboard's in-process [`SpanBuffer`](super::buffer::SpanBuffer) and the
//! durable [`SpanStore`](super::SpanStore) backends. Each destination
//! installs its own [`RecordingLayer`] with its own [`SpanRecordSink`], so
//! adding a new destination does not perturb existing ones.

use super::SpanKind;
use super::SpanRecord;
#[cfg(feature = "dashboard")]
use super::buffer::SpanBuffer;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

const RUN_ID_FIELD: &str = "polaris.run.id";
const LABEL_FIELD_PREFIX: &str = "polaris.label.";

/// Sink for [`SpanRecord`]s produced by [`RecordingLayer`].
///
/// The same recording layer can fan records into multiple destinations — the
/// in-process ring ([`SpanBuffer`]), a durable [`SpanStore`](super::SpanStore)
/// backend, an external exporter — by installing one [`RecordingLayer`] per
/// sink. This keeps each sink a small, composable plugin: no plugin needs to
/// know about another's storage strategy, and adding a new destination does
/// not perturb existing ones.
pub trait SpanRecordSink: Send + Sync + 'static {
    /// Receives a freshly-built record. Sinks must be cheap and non-blocking;
    /// any I/O should be deferred to a background task.
    fn push(&self, record: SpanRecord);
}

/// Tracing layer that captures recent events and span-close records into a
/// [`SpanRecordSink`].
///
/// The layer emits records only for `on_event` and `on_close`. It does use
/// `on_new_span` and `on_record` internally to cache span fields so the close
/// record can include them, but it intentionally does not emit records for
/// span creation or enter/exit. Expect roughly 5-10 µs of additional work per
/// event when this feature is enabled.
///
/// The sink is decoupled from the layer so the same recording machinery can
/// power both the dashboard's in-process [`SpanBuffer`] and the persistent
/// [`SpanStore`](super::SpanStore) backends without duplicating visitor
/// logic.
#[derive(Clone)]
pub struct RecordingLayer {
    sink: Arc<dyn SpanRecordSink>,
}

impl std::fmt::Debug for RecordingLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingLayer").finish_non_exhaustive()
    }
}

impl RecordingLayer {
    /// Creates a new layer that writes into the provided [`SpanBuffer`].
    #[cfg(feature = "dashboard")]
    #[must_use]
    pub fn new(buffer: SpanBuffer) -> Self {
        Self {
            sink: Arc::new(buffer),
        }
    }

    /// Creates a new layer that forwards records to an arbitrary
    /// [`SpanRecordSink`].
    ///
    /// Used by [`SpanStorePlugin`](super::SpanStorePlugin) to attach a
    /// durable-storage sink without coupling the layer to a specific backend.
    #[must_use]
    pub fn with_sink(sink: Arc<dyn SpanRecordSink>) -> Self {
        Self { sink }
    }
}

impl Layer<tracing_subscriber::Registry> for RecordingLayer {
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, tracing_subscriber::Registry>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };

        // Multiple `RecordingLayer` instances can coexist on the same
        // subscriber (e.g. one routing into the dashboard's `SpanBuffer`,
        // another routing into a `SpanStore` for durability). They all
        // need the same captured-data view on a span, but
        // `tracing-subscriber`'s per-span `Extensions` map panics on a
        // duplicate `insert` of the same type. The first layer to see a
        // span owns the extension; subsequent layers reuse it.
        if span.extensions().get::<CapturedSpanData>().is_some() {
            return;
        }

        let mut visitor = JsonVisitor::default();
        attrs.record(&mut visitor);
        let mut data = CapturedSpanData::from(visitor);
        // Inherit run_id from the parent chain when this span did not
        // declare one on construction.
        if data.run_id.is_none()
            && let Some(parent) = span.parent()
        {
            data.run_id = inherited_run_id(&parent);
        }
        data.opened_at_wall = Some(Utc::now());
        data.opened_at_mono = Some(Instant::now());
        span.extensions_mut().insert(data);
    }

    fn on_record(
        &self,
        id: &Id,
        values: &Record<'_>,
        ctx: Context<'_, tracing_subscriber::Registry>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };

        let mut visitor = JsonVisitor::default();
        values.record(&mut visitor);

        let mut extensions = span.extensions_mut();
        if let Some(data) = extensions.get_mut::<CapturedSpanData>() {
            data.merge(visitor);
        } else {
            extensions.insert(CapturedSpanData::from(visitor));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, tracing_subscriber::Registry>) {
        let metadata = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        let event_span = ctx.event_span(event);
        let name = event_span.as_ref().map_or_else(
            || metadata.name().to_string(),
            |span| span.name().to_string(),
        );

        let span_id = event_span
            .as_ref()
            .map(|span| span.id().into_u64().to_string());
        let parent_span_id = event_span.as_ref().and_then(|span| {
            span.parent()
                .map(|parent| parent.id().into_u64().to_string())
        });

        let run_id = visitor
            .run_id
            .clone()
            .or_else(|| event_span.as_ref().and_then(|span| inherited_run_id(span)));

        let mut labels = visitor.labels;
        if let Some(span) = event_span.as_ref() {
            inherit_labels(span, &mut labels);
        }

        self.sink.push(SpanRecord {
            ts: timestamp_now(),
            started_at: None,
            duration_ms: None,
            level: level_label(metadata.level()),
            target: metadata.target().to_string(),
            name,
            kind: SpanKind::Event,
            span_id,
            parent_span_id,
            run_id,
            labels,
            fields: visitor.fields,
            message: visitor.message,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, tracing_subscriber::Registry>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };

        let metadata = span.metadata();
        let extensions = span.extensions();
        let (fields, message, run_id, opened_at_wall, opened_at_mono) =
            extensions.get::<CapturedSpanData>().map_or_else(
                || (Map::new(), None, None, None, None),
                |data| {
                    (
                        data.fields.clone(),
                        data.message.clone(),
                        data.run_id.clone(),
                        data.opened_at_wall,
                        data.opened_at_mono,
                    )
                },
            );
        drop(extensions);

        // Fall back to ancestor run_id if the span didn't carry one.
        let resolved_run_id = run_id.or_else(|| inherited_run_id(&span));

        let mut labels = BTreeMap::new();
        inherit_labels(&span, &mut labels);

        let duration_ms = opened_at_mono.map(|start| {
            let elapsed = start.elapsed();
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
        let started_at = opened_at_wall.map(datetime_to_string);

        let parent_span_id = span
            .parent()
            .map(|parent| parent.id().into_u64().to_string());

        self.sink.push(SpanRecord {
            ts: timestamp_now(),
            started_at,
            duration_ms,
            level: level_label(metadata.level()),
            target: metadata.target().to_string(),
            name: span.name().to_string(),
            kind: SpanKind::SpanClose,
            span_id: Some(id.into_u64().to_string()),
            parent_span_id,
            run_id: resolved_run_id,
            labels,
            fields,
            message,
        });
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct CapturedSpanData {
    fields: Map<String, Value>,
    message: Option<String>,
    /// Inherited or self-declared `run_id`. Set during `on_new_span`/`on_record`
    /// and emitted on `on_close`.
    run_id: Option<String>,
    /// Correlation labels (`polaris.label.<key>` fields) inherited or
    /// captured on this span. Merged with ancestor labels on emit.
    labels: BTreeMap<String, String>,
    /// Recorded at `on_new_span` so close records can report duration.
    opened_at_wall: Option<DateTime<Utc>>,
    opened_at_mono: Option<Instant>,
}

impl CapturedSpanData {
    fn merge(&mut self, visitor: JsonVisitor) {
        if let Some(message) = visitor.message {
            self.message = Some(message);
        }
        if let Some(run_id) = visitor.run_id {
            self.run_id = Some(run_id);
        }
        for (key, value) in visitor.labels {
            self.labels.insert(key, value);
        }
        for (key, value) in visitor.fields {
            self.fields.insert(key, value);
        }
    }
}

impl From<JsonVisitor> for CapturedSpanData {
    fn from(visitor: JsonVisitor) -> Self {
        Self {
            fields: visitor.fields,
            message: visitor.message,
            run_id: visitor.run_id,
            labels: visitor.labels,
            opened_at_wall: None,
            opened_at_mono: None,
        }
    }
}

#[derive(Debug, Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
    message: Option<String>,
    /// `polaris.run.id` extracted into a typed slot. Captured here because
    /// the span buffer treats it as a top-level correlation key.
    run_id: Option<String>,
    /// Correlation labels extracted from any field whose name starts with
    /// the `polaris.label.` prefix.
    labels: BTreeMap<String, String>,
}

/// Stringifies a [`Value`] for storage in the labels map.
fn label_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl JsonVisitor {
    fn insert_value(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = value.as_str().map(ToOwned::to_owned).or_else(|| {
                if matches!(value, Value::String(_)) {
                    None
                } else {
                    Some(value.to_string())
                }
            });
            return;
        }

        if field.name() == RUN_ID_FIELD {
            self.run_id = value.as_str().map(ToOwned::to_owned);
            self.fields.insert(field.name().to_string(), value);
            return;
        }

        if let Some(key) = field.name().strip_prefix(LABEL_FIELD_PREFIX) {
            self.labels.insert(key.to_owned(), label_string(&value));
            self.fields.insert(field.name().to_string(), value);
            return;
        }

        self.fields.insert(field.name().to_string(), value);
    }

    fn insert_string(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
            return;
        }

        if field.name() == RUN_ID_FIELD {
            self.run_id = Some(value.clone());
            self.fields
                .insert(field.name().to_string(), Value::String(value));
            return;
        }

        if let Some(key) = field.name().strip_prefix(LABEL_FIELD_PREFIX) {
            self.labels.insert(key.to_owned(), value.clone());
            self.fields
                .insert(field.name().to_string(), Value::String(value));
            return;
        }

        self.fields
            .insert(field.name().to_string(), Value::String(value));
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        match Number::from_f64(value) {
            Some(number) => self.insert_value(field, Value::Number(number)),
            None => self.insert_string(field, value.to_string()),
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert_value(field, Value::Number(Number::from(value)));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert_value(field, Value::Number(Number::from(value)));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        match Number::from_i128(value) {
            Some(number) => self.insert_value(field, Value::Number(number)),
            None => self.insert_string(field, value.to_string()),
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        match Number::from_u128(value) {
            Some(number) => self.insert_value(field, Value::Number(number)),
            None => self.insert_string(field, value.to_string()),
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert_value(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert_string(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert_string(field, format!("{value:?}"));
    }
}

fn level_label(level: &Level) -> String {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
    .to_owned()
}

pub(super) fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn datetime_to_string(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Walks ancestor span data to discover an inherited `run_id`.
fn inherited_run_id<S>(span: &tracing_subscriber::registry::SpanRef<'_, S>) -> Option<String>
where
    S: for<'a> LookupSpan<'a>,
{
    if let Some(data) = span.extensions().get::<CapturedSpanData>()
        && let Some(rid) = data.run_id.as_ref()
    {
        return Some(rid.clone());
    }
    let mut current = span.parent();
    while let Some(node) = current {
        if let Some(data) = node.extensions().get::<CapturedSpanData>()
            && let Some(rid) = data.run_id.as_ref()
        {
            return Some(rid.clone());
        }
        current = node.parent();
    }
    None
}

/// Walks the ancestor chain merging labels from each scope into `acc`.
///
/// Inner scopes take precedence over outer scopes (labels already present
/// in `acc` are not overwritten), so a span that re-declares a label is
/// honoured.
fn inherit_labels<S>(
    span: &tracing_subscriber::registry::SpanRef<'_, S>,
    acc: &mut BTreeMap<String, String>,
) where
    S: for<'a> LookupSpan<'a>,
{
    if let Some(data) = span.extensions().get::<CapturedSpanData>() {
        for (key, value) in &data.labels {
            acc.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    let mut current = span.parent();
    while let Some(node) = current {
        if let Some(data) = node.extensions().get::<CapturedSpanData>() {
            for (key, value) in &data.labels {
                acc.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        current = node.parent();
    }
}

#[cfg(all(test, feature = "dashboard"))]
mod tests {
    use super::*;
    use serde_json::Value;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn layer_records_event_with_fields_and_message() {
        let buffer = SpanBuffer::with_capacity(8);
        let subscriber = tracing_subscriber::registry().with(RecordingLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(answer = 42, "hello world");
        });

        let snapshot = buffer.snapshot(usize::MAX);
        assert_eq!(snapshot.len(), 1);
        let record = &snapshot[0];
        assert_eq!(record.kind, SpanKind::Event);
        assert_eq!(record.level, "info");
        assert_eq!(record.message.as_deref(), Some("hello world"));
        assert_eq!(
            record.fields.get("answer").and_then(Value::as_i64),
            Some(42),
        );
    }

    #[test]
    fn layer_emits_close_record_with_span_fields() {
        let buffer = SpanBuffer::with_capacity(8);
        let subscriber = tracing_subscriber::registry().with(RecordingLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("work", job = "compile");
            drop(span.enter());
            drop(span);
        });

        let snapshot = buffer.snapshot(usize::MAX);
        let close = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::SpanClose)
            .expect("layer must emit a SpanClose record");
        assert_eq!(close.name, "work");
        assert_eq!(
            close.fields.get("job").and_then(Value::as_str),
            Some("compile"),
        );
        assert!(close.span_id.is_some(), "span_id must be set for closes");
        assert!(
            close.started_at.is_some(),
            "started_at must be captured for closes"
        );
        assert!(
            close.duration_ms.is_some(),
            "duration_ms must be captured for closes"
        );
    }

    #[test]
    fn layer_propagates_run_id_from_parent_to_child_spans_and_events() {
        let buffer = SpanBuffer::with_capacity(32);
        let subscriber = tracing_subscriber::registry().with(RecordingLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("outer", polaris.run.id = "run-A");
            let _g = outer.enter();
            let inner = tracing::info_span!("inner", job = "child");
            let _g2 = inner.enter();
            tracing::info!(message = "nested event");
        });

        let snapshot = buffer.snapshot(usize::MAX);
        let event = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::Event)
            .expect("event must be captured");
        assert_eq!(
            event.run_id.as_deref(),
            Some("run-A"),
            "inner event must inherit run_id from ancestor span"
        );
        let inner_close = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::SpanClose && record.name == "inner")
            .expect("inner span must close");
        assert_eq!(
            inner_close.run_id.as_deref(),
            Some("run-A"),
            "child span must inherit run_id from parent"
        );
    }

    #[test]
    fn visitor_extracts_polaris_label_fields_into_labels_map() {
        let buffer = SpanBuffer::with_capacity(8);
        let subscriber = tracing_subscriber::registry().with(RecordingLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "session.turn",
                polaris.label.session_id = "sess-1",
                polaris.label.turn = 7_u64,
            );
            drop(span.enter());
            drop(span);
        });

        let snapshot = buffer.snapshot(usize::MAX);
        let close = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::SpanClose)
            .expect("close record");
        assert_eq!(
            close.labels.get("session_id").map(String::as_str),
            Some("sess-1"),
            "polaris.label.session_id must populate labels[\"session_id\"]"
        );
        assert_eq!(
            close.labels.get("turn").map(String::as_str),
            Some("7"),
            "non-string label values must be stringified"
        );
    }

    #[test]
    fn child_records_inherit_labels_from_parent_span() {
        let buffer = SpanBuffer::with_capacity(16);
        let subscriber = tracing_subscriber::registry().with(RecordingLayer::new(buffer.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("outer", polaris.label.session_id = "sess-X");
            let _g = outer.enter();
            let inner = tracing::info_span!("inner");
            let _g2 = inner.enter();
            tracing::info!("nested event");
        });

        let snapshot = buffer.snapshot(usize::MAX);
        let inner_close = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::SpanClose && record.name == "inner")
            .expect("inner span must close");
        assert_eq!(
            inner_close.labels.get("session_id").map(String::as_str),
            Some("sess-X"),
            "child span must inherit labels from parent"
        );
        let event = snapshot
            .iter()
            .find(|record| record.kind == SpanKind::Event)
            .expect("event must be recorded");
        assert_eq!(
            event.labels.get("session_id").map(String::as_str),
            Some("sess-X"),
            "nested event must inherit labels from ancestor span"
        );
    }
}
