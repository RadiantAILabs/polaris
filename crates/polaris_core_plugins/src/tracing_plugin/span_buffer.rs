//! Span ring buffer and tracing layer for the dashboard endpoint.

use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use polaris_system::api::API;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Ring buffer of recent tracing records for the dashboard endpoint.
///
/// The buffer is `Arc`-backed and uses a `parking_lot::Mutex<VecDeque<_>>`
/// internally. [`TracingDashboardPlugin`](super::TracingDashboardPlugin)
/// inserts it as a build-time API so the HTTP handler can snapshot the most
/// recent records without reaching into tracing internals.
#[derive(Debug, Clone)]
pub struct SpanBuffer {
    capacity: usize,
    records: Arc<Mutex<VecDeque<SpanRecord>>>,
}

impl API for SpanBuffer {}

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

        let mut guard = self.records.lock();
        if guard.len() >= self.capacity {
            let _ = guard.pop_front();
        }
        guard.push_back(record);
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
}

impl Default for SpanBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Wire kind for tracing records emitted by [`SpanBufferLayer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpanKind {
    /// A `tracing::event!` record.
    Event,
    /// Emitted when a span closes.
    SpanClose,
}

/// Wire representation of a recent tracing record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanRecord {
    /// ISO-8601 UTC timestamp.
    pub ts: String,
    /// Lower-cased tracing level (`info`, `warn`, `error`, ...).
    pub level: String,
    /// Tracing metadata target.
    pub target: String,
    /// Event name or the associated span name.
    pub name: String,
    /// Record kind.
    pub kind: SpanKind,
    /// Structured fields captured from the event or span.
    pub fields: Map<String, Value>,
    /// Optional message field extracted from the tracing payload.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CapturedSpanData {
    fields: Map<String, Value>,
    message: Option<String>,
}

impl CapturedSpanData {
    fn merge(&mut self, visitor: JsonVisitor) {
        if let Some(message) = visitor.message {
            self.message = Some(message);
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
        }
    }
}

#[derive(Debug, Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
    message: Option<String>,
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

        self.fields.insert(field.name().to_string(), value);
    }

    fn insert_string(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
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

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Tracing layer that captures recent events and span-close records.
///
/// The layer emits records only for `on_event` and `on_close`. It does use
/// `on_new_span` and `on_record` internally to cache span fields so the close
/// record can include them, but it intentionally does not emit records for
/// span creation or enter/exit. Expect roughly 5-10 µs of additional work per
/// event when this feature is enabled.
#[derive(Debug, Clone)]
pub struct SpanBufferLayer {
    buffer: SpanBuffer,
}

impl SpanBufferLayer {
    /// Creates a new layer that writes into the provided [`SpanBuffer`].
    #[must_use]
    pub fn new(buffer: SpanBuffer) -> Self {
        Self { buffer }
    }
}

impl Layer<tracing_subscriber::Registry> for SpanBufferLayer {
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, tracing_subscriber::Registry>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };

        let mut visitor = JsonVisitor::default();
        attrs.record(&mut visitor);
        span.extensions_mut()
            .insert(CapturedSpanData::from(visitor));
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

        let name = ctx.event_span(event).map_or_else(
            || metadata.name().to_string(),
            |span| span.name().to_string(),
        );

        self.buffer.push(SpanRecord {
            ts: timestamp_now(),
            level: level_label(metadata.level()),
            target: metadata.target().to_string(),
            name,
            kind: SpanKind::Event,
            fields: visitor.fields,
            message: visitor.message,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, tracing_subscriber::Registry>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };

        let metadata = span.metadata();
        let (fields, message) = span.extensions().get::<CapturedSpanData>().map_or_else(
            || (Map::new(), None),
            |data| (data.fields.clone(), data.message.clone()),
        );

        self.buffer.push(SpanRecord {
            ts: timestamp_now(),
            level: level_label(metadata.level()),
            target: metadata.target().to_string(),
            name: span.name().to_string(),
            kind: SpanKind::SpanClose,
            fields,
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    fn make_record(idx: usize) -> SpanRecord {
        SpanRecord {
            ts: idx.to_string(),
            level: "info".to_owned(),
            target: "tests".into(),
            name: format!("record-{idx}"),
            kind: SpanKind::Event,
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

    #[test]
    fn layer_records_event_with_fields_and_message() {
        let buffer = SpanBuffer::with_capacity(8);
        let subscriber = tracing_subscriber::registry().with(SpanBufferLayer::new(buffer.clone()));

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
        let subscriber = tracing_subscriber::registry().with(SpanBufferLayer::new(buffer.clone()));

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
    }
}
