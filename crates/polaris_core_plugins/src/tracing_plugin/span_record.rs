//! Wire types for one captured tracing record.
//!
//! These types are intentionally feature-flag-free: they are the contract
//! between the `dashboard`'s in-memory [`SpanBuffer`](super::SpanBuffer)
//! and the durable [`SpanStore`](super::SpanStore). Either side of that
//! pipe — the layer that produces records or the backend that persists
//! them — can be enabled independently of the other, so the types they
//! share live at the bottom of the gate hierarchy.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Wire kind for tracing records emitted by
/// [`RecordingLayer`](super::RecordingLayer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export, rename_all = "kebab-case"))]
#[serde(rename_all = "kebab-case")]
pub enum SpanKind {
    /// A `tracing::event!` record.
    Event,
    /// Emitted when a span closes.
    SpanClose,
}

/// Wire representation of a recent tracing record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[non_exhaustive]
pub struct SpanRecord {
    /// ISO-8601 UTC timestamp the record was emitted (span close time or
    /// event time).
    pub ts: String,
    /// ISO-8601 UTC timestamp the span started. `None` for `Event`
    /// records (events have no separate start) and for spans that did not
    /// pass through the dashboard layer's open path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Closed-span duration in milliseconds. `None` for `Event` records or
    /// spans missing a recorded open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    /// Lower-cased tracing level (`info`, `warn`, `error`, ...).
    pub level: String,
    /// Tracing metadata target.
    pub target: String,
    /// Event name or the associated span name.
    pub name: String,
    /// Record kind.
    pub kind: SpanKind,
    /// Stable per-process span identifier. `None` for `Event` records that
    /// fired outside any span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Parent span identifier when the record is nested under another span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    /// Run identifier (`run_id` from `polaris_graph` hooks), propagated to
    /// every span and event that fires under a graph execution. `None`
    /// outside graph execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Correlation labels captured for this record.
    ///
    /// Populated from any tracing field whose name starts with the
    /// `polaris.label.` prefix — the suffix becomes the label key and the
    /// stringified value becomes the label value. Labels are inherited
    /// down the parent span chain, so a label set on a session turn span
    /// surfaces on every nested graph span and event.
    ///
    /// Conventional keys include `session_id`, `agent_type`, and `turn`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, string>"))]
    pub labels: BTreeMap<String, String>,
    /// Structured fields captured from the event or span.
    #[cfg_attr(feature = "typegen", ts(type = "Record<string, unknown>"))]
    pub fields: Map<String, Value>,
    /// Optional message field extracted from the tracing payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl SpanRecord {
    /// Constructs a minimal record. Optional fields default to `None` /
    /// empty so callers can chain `with_*` setters to populate the parts
    /// they need.
    #[must_use]
    pub fn new(
        ts: impl Into<String>,
        level: impl Into<String>,
        target: impl Into<String>,
        name: impl Into<String>,
        kind: SpanKind,
    ) -> Self {
        Self {
            ts: ts.into(),
            started_at: None,
            duration_ms: None,
            level: level.into(),
            target: target.into(),
            name: name.into(),
            kind,
            span_id: None,
            parent_span_id: None,
            run_id: None,
            labels: BTreeMap::new(),
            fields: Map::new(),
            message: None,
        }
    }

    /// Sets [`SpanRecord::started_at`].
    #[must_use]
    pub fn with_started_at(mut self, started_at: impl Into<String>) -> Self {
        self.started_at = Some(started_at.into());
        self
    }

    /// Sets [`SpanRecord::duration_ms`].
    #[must_use]
    pub const fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Sets [`SpanRecord::span_id`].
    #[must_use]
    pub fn with_span_id(mut self, span_id: impl Into<String>) -> Self {
        self.span_id = Some(span_id.into());
        self
    }

    /// Sets [`SpanRecord::parent_span_id`].
    #[must_use]
    pub fn with_parent_span_id(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    /// Sets [`SpanRecord::run_id`].
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    /// Sets [`SpanRecord::message`].
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Inserts a structured field, returning the updated record.
    #[must_use]
    pub fn with_field(mut self, key: impl Into<String>, value: Value) -> Self {
        self.fields.insert(key.into(), value);
        self
    }

    /// Inserts a correlation label, returning the updated record.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }
}
