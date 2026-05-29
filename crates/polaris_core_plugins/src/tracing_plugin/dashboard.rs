//! HTTP endpoints for the tracing UI.
//!
//! - `GET /v1/tracing/spans` — flat tail of recent records.
//! - `GET /v1/tracing/runs` — distinct runs observed in [`SpanBuffer`],
//!   most-recent-first.
//! - `GET /v1/tracing/runs/{run_id}` — hierarchical [`SpanTree`] for a
//!   single run. Default response **embeds event payloads inside the
//!   tree**; pass `?include=structure` for a payload-free shape suitable
//!   for lazy loading.
//! - `GET /v1/tracing/runs/{run_id}/spans/{span_id}` — single-span
//!   payload fetch, used by the frontend when the tree was loaded in
//!   structure-only mode.
//! - `GET /v1/tracing/sessions` — distinct sessions observed in
//!   [`SpanBuffer`], independent of live-session-store membership.
//!   Surfaces ephemeral one-shot sessions that have already been
//!   reclaimed from the sessions store, so the dashboard retains a
//!   navigable entry point to their tracing data. Pair with
//!   [`SpanStorePlugin`](super::SpanStorePlugin) to extend the surface
//!   across process restarts.
//! - `GET /v1/tracing/usage` — buffer-wide token usage rollup, optionally
//!   filtered by `?label=key:value`. Companion endpoints exist for
//!   per-run (`/v1/tracing/runs/{run_id}/usage`) and per-session
//!   (`/v1/sessions/{session_id}/usage`,
//!   `/v1/sessions/{session_id}/runs/{run_id}/usage`) scopes.
//!
//! The run-tree default is `include=payloads`. Against synthetic
//! ReAct-shaped trees (see `tests/tracing_run_tree_p95.rs`), p95 wire
//! size measures ~790 KiB — comfortably under the 1 MiB ceiling. Flip
//! via `?include=structure` per request when the frontend prefers to
//! load the tree shape only and fetch payloads lazily.

use super::{
    RecordingLayer, RunSummary, SessionSummary, SpanBuffer, SpanRecord, SpanTree,
    TokenUsageResponse, TracingLayers, TreeView, UsagePricing,
};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use polaris_app::HttpRouter;
use polaris_system::server::Server;
use serde::{Deserialize, Serialize};

const DEFAULT_SPAN_LIMIT: usize = 200;
const DEFAULT_RUN_LIMIT: usize = 100;
const DEFAULT_SESSION_LIMIT: usize = 100;
const TRACING_SPANS_PATH: &str = "/v1/tracing/spans";
const TRACING_RUNS_PATH: &str = "/v1/tracing/runs";
/// Tracing-known sessions endpoint. Distinct from `/v1/sessions`, which
/// lists only sessions still alive in the [`SessionsAPI`] store. This
/// surface is backed by [`SpanBuffer::distinct_sessions`] and therefore
/// includes ephemeral sessions whose store entry has already been
/// reclaimed — e.g. one-shot runs from
/// [`SessionsAPI::run_oneshot`](polaris_sessions::SessionsAPI::run_oneshot).
///
/// [`SessionsAPI`]: polaris_sessions::SessionsAPI
const TRACING_SESSIONS_PATH: &str = "/v1/tracing/sessions";
const TRACING_RUN_TREE_PATH: &str = "/v1/tracing/runs/{run_id}";
const TRACING_SPAN_PATH: &str = "/v1/tracing/runs/{run_id}/spans/{span_id}";

/// Buffer-wide token usage rollup. Accepts an optional `?label=key:value`
/// query parameter that filters the aggregation to records carrying that
/// correlation label.
const TRACING_USAGE_PATH: &str = "/v1/tracing/usage";
/// Per-run token usage rollup.
const TRACING_RUN_USAGE_PATH: &str = "/v1/tracing/runs/{run_id}/usage";
/// Session-scoped token usage rollup, summed across every run for the
/// session that's still in the buffer.
const SESSIONS_USAGE_PATH: &str = "/v1/sessions/{session_id}/usage";
/// Session-scoped per-run token usage rollup, gated on session membership.
const SESSIONS_RUN_USAGE_PATH: &str = "/v1/sessions/{session_id}/runs/{run_id}/usage";

/// Session-scoped run list endpoint. Wraps
/// [`SpanBuffer::distinct_runs_by_label`] filtered on `session_id`.
const SESSIONS_RUNS_PATH: &str = "/v1/sessions/{session_id}/runs";
/// Session-scoped run-tree endpoint. Wraps [`SpanBuffer::run_tree`],
/// validating that the run belongs to the named session before returning.
const SESSIONS_RUN_TREE_PATH: &str = "/v1/sessions/{session_id}/runs/{run_id}/tree";
/// Session-scoped per-span endpoint. Mirrors [`TRACING_SPAN_PATH`] with the
/// same session-membership validation.
const SESSIONS_SPAN_PATH: &str = "/v1/sessions/{session_id}/runs/{run_id}/spans/{span_id}";

/// Label key used to join a run back to its owning session.
const SESSION_LABEL_KEY: &str = "session_id";

#[derive(Debug, Default, Deserialize)]
struct SpansQuery {
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RunsQuery {
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionsQuery {
    limit: Option<usize>,
}

/// Selector for the `?include=` query parameter on the run-tree endpoint.
#[derive(Debug, Default, Deserialize)]
struct RunTreeQuery {
    /// One of `payloads` (default) or `structure`. Absent means the
    /// default; an unrecognized value is rejected by the extractor with
    /// `400 Bad Request` rather than silently serving payloads.
    include: Option<TreeView>,
}

/// Selector for the `?label=key:value` query parameter on the
/// buffer-wide usage endpoint.
#[derive(Debug, Default, Deserialize)]
struct UsageQuery {
    /// `key:value` correlation label filter. Records whose `labels` map
    /// contains the matching entry are included. Absent means no filter;
    /// a malformed value (missing `:` or empty key) is rejected by the
    /// extractor with `400 Bad Request`.
    label: Option<LabelFilter>,
}

/// A parsed `key:value` correlation-label filter.
///
/// Deserializes from the raw `key:value` query string via [`TryFrom`], so
/// a malformed filter is rejected at the extractor boundary (yielding a
/// `400`) instead of being silently dropped to an unfiltered aggregate.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
struct LabelFilter {
    key: String,
    value: String,
}

impl TryFrom<String> for LabelFilter {
    type Error = &'static str;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let (key, value) = raw
            .split_once(':')
            .ok_or("label filter must be in `key:value` form")?;
        if key.is_empty() {
            return Err("label filter key must not be empty");
        }
        Ok(Self {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }
}

impl<'de> Deserialize<'de> for LabelFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Shared axum state for the four usage handlers.
#[derive(Clone)]
struct UsageState {
    buffer: SpanBuffer,
    pricing: UsagePricing,
}

/// Wire response for `GET /v1/tracing/spans`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpansResponse {
    /// Recent tracing records, oldest-to-newest within the selected window.
    pub items: Vec<SpanRecord>,
}

/// Wire response for `GET /v1/tracing/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunsResponse {
    /// Distinct runs observed in the buffer, most-recent first.
    pub items: Vec<RunSummary>,
}

/// Wire response for `GET /v1/tracing/sessions`.
///
/// Surfaces every session id that has produced spans in the buffer (and,
/// when [`SpanStorePlugin`](super::SpanStorePlugin) hydrated from a
/// store, every session id known to that store). Distinct from the
/// `GET /v1/sessions` list, which only reports sessions currently alive
/// in the [`SessionsAPI`](polaris_sessions::SessionsAPI) store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionsResponse {
    /// Distinct sessions observed in the buffer, most-recent first.
    pub items: Vec<SessionSummary>,
}

/// Installs the dashboard span buffer, usage-pricing API, recording layer,
/// and all `/v1/tracing/*` + `/v1/sessions/{id}/...` HTTP routes during
/// [`TracingPlugin::build`](super::TracingPlugin).
///
/// Endpoints mounted:
///
/// | Method | Path | Returns |
/// |--------|------|---------|
/// | `GET` | `/v1/tracing/spans?limit=N` | Flat tail of recent records. |
/// | `GET` | `/v1/tracing/runs?limit=N` | Distinct runs in the buffer. |
/// | `GET` | `/v1/tracing/runs/{run_id}?include=structure\|payloads` | Hierarchical [`SpanTree`] for the run. |
/// | `GET` | `/v1/tracing/runs/{run_id}/spans/{span_id}` | One span's close record (payload lookup). |
/// | `GET` | `/v1/tracing/sessions?limit=N` | Distinct sessions observed in the buffer, independent of live-session-store membership. |
/// | `GET` | `/v1/tracing/usage?label=key:value` | Token usage rollup across the whole buffer, optionally filtered by correlation label. |
/// | `GET` | `/v1/tracing/runs/{run_id}/usage` | Token usage rollup for one run. |
/// | `GET` | `/v1/sessions/{session_id}/runs` | Runs filtered by the `session_id` label, most-recent-first. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/tree?include=structure\|payloads` | Hierarchical [`SpanTree`] for a run, gated on session membership. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/spans/{span_id}` | One span's close record, gated on session membership. |
/// | `GET` | `/v1/sessions/{session_id}/usage` | Token usage rollup summed across every run for the session. |
/// | `GET` | `/v1/sessions/{session_id}/runs/{run_id}/usage` | Token usage rollup for one run, gated on session membership. |
///
/// The run-tree endpoint defaults to `include=payloads`. Pass
/// `?include=structure` when the frontend wants to load the tree shape only
/// and fetch payloads lazily via the per-span endpoint.
///
/// The `/v1/sessions/{session_id}/...` family mirrors the corresponding
/// `/v1/tracing/...` endpoint with the addition of a `session_id`
/// membership check derived from the `polaris.label.*` fields propagated
/// by [`SessionsAPI`](polaris_sessions::SessionsAPI). A run whose labels
/// do not contain the named `session_id` returns 404.
pub(crate) fn install(server: &mut Server) {
    let buffer = SpanBuffer::with_capacity(SpanBuffer::DEFAULT_CAPACITY);
    server.insert_api(buffer.clone());
    server.insert_api(UsagePricing::default());

    server
        .get_resource_mut::<TracingLayers>()
        .expect("TracingLayers must exist (TracingPlugin registers it in build)")
        .push(RecordingLayer::new(buffer));

    server
        .api::<HttpRouter>()
        .expect("AppPlugin must be added before TracingPlugin when `dashboard` is enabled")
        .add_routes_with(|server| {
            let buffer = server
                .api::<SpanBuffer>()
                .expect("SpanBuffer must exist (registered in build)")
                .clone();
            let pricing = server
                .api::<UsagePricing>()
                .expect("UsagePricing must exist (registered in build)")
                .clone();
            let usage_state = UsageState {
                buffer: buffer.clone(),
                pricing,
            };
            Router::new()
                .route(TRACING_SPANS_PATH, get(spans_handler))
                .route(TRACING_RUNS_PATH, get(runs_handler))
                .route(TRACING_RUN_TREE_PATH, get(run_tree_handler))
                .route(TRACING_SPAN_PATH, get(span_handler))
                .route(TRACING_SESSIONS_PATH, get(tracing_sessions_handler))
                .route(SESSIONS_RUNS_PATH, get(sessions_runs_handler))
                .route(SESSIONS_RUN_TREE_PATH, get(sessions_run_tree_handler))
                .route(SESSIONS_SPAN_PATH, get(sessions_span_handler))
                .with_state(buffer)
                .merge(
                    Router::new()
                        .route(TRACING_USAGE_PATH, get(usage_handler))
                        .route(TRACING_RUN_USAGE_PATH, get(run_usage_handler))
                        .route(SESSIONS_USAGE_PATH, get(sessions_usage_handler))
                        .route(SESSIONS_RUN_USAGE_PATH, get(sessions_run_usage_handler))
                        .with_state(usage_state),
                )
        });
}

/// `GET /v1/tracing/spans` — returns recent tracing records.
async fn spans_handler(
    State(buffer): State<SpanBuffer>,
    Query(query): Query<SpansQuery>,
) -> Json<SpansResponse> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SPAN_LIMIT)
        .min(buffer.capacity());

    Json(SpansResponse {
        items: buffer.snapshot(limit),
    })
}

/// `GET /v1/tracing/runs` — returns distinct runs observed in the buffer.
async fn runs_handler(
    State(buffer): State<SpanBuffer>,
    Query(query): Query<RunsQuery>,
) -> Json<RunsResponse> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RUN_LIMIT)
        .min(buffer.capacity());

    Json(RunsResponse {
        items: buffer.distinct_runs(limit),
    })
}

/// `GET /v1/tracing/sessions` — returns distinct sessions observed in
/// the buffer, regardless of live-session-store membership.
async fn tracing_sessions_handler(
    State(buffer): State<SpanBuffer>,
    Query(query): Query<SessionsQuery>,
) -> Json<SessionsResponse> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SESSION_LIMIT)
        .min(buffer.capacity());

    Json(SessionsResponse {
        items: buffer.distinct_sessions(limit),
    })
}

/// `GET /v1/tracing/runs/{run_id}` — returns the hierarchical span tree.
///
/// Default response embeds event payloads. `?include=structure` strips them.
async fn run_tree_handler(
    State(buffer): State<SpanBuffer>,
    Path(run_id): Path<String>,
    Query(query): Query<RunTreeQuery>,
) -> Response {
    let view = query.include.unwrap_or(TreeView::Payloads);
    match buffer.run_tree(&run_id, view) {
        Some(tree) => Json::<SpanTree>(tree).into_response(),
        None => (StatusCode::NOT_FOUND, "run not found in buffer").into_response(),
    }
}

/// `GET /v1/tracing/runs/{run_id}/spans/{span_id}` — single-span payload.
async fn span_handler(
    State(buffer): State<SpanBuffer>,
    Path((run_id, span_id)): Path<(String, String)>,
) -> Response {
    match buffer.span(&run_id, &span_id) {
        Some(record) => Json::<SpanRecord>(record).into_response(),
        None => (StatusCode::NOT_FOUND, "span not found in buffer").into_response(),
    }
}

/// `GET /v1/sessions/{session_id}/runs` — runs filtered by `session_id`
/// label.
async fn sessions_runs_handler(
    State(buffer): State<SpanBuffer>,
    Path(session_id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Json<RunsResponse> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RUN_LIMIT)
        .min(buffer.capacity());

    Json(RunsResponse {
        items: buffer.distinct_runs_by_label(SESSION_LABEL_KEY, &session_id, limit),
    })
}

/// `GET /v1/sessions/{session_id}/runs/{run_id}/tree` — hierarchical span
/// tree for a run, gated on session membership.
async fn sessions_run_tree_handler(
    State(buffer): State<SpanBuffer>,
    Path((session_id, run_id)): Path<(String, String)>,
    Query(query): Query<RunTreeQuery>,
) -> Response {
    let view = query.include.unwrap_or(TreeView::Payloads);
    match buffer.run_tree(&run_id, view) {
        Some(tree)
            if tree
                .labels
                .get(SESSION_LABEL_KEY)
                .is_some_and(|value| value == &session_id) =>
        {
            Json::<SpanTree>(tree).into_response()
        }
        Some(_) | None => {
            (StatusCode::NOT_FOUND, "run not found for session in buffer").into_response()
        }
    }
}

/// `GET /v1/sessions/{session_id}/runs/{run_id}/spans/{span_id}` — single
/// span record, gated on session membership.
async fn sessions_span_handler(
    State(buffer): State<SpanBuffer>,
    Path((session_id, run_id, span_id)): Path<(String, String, String)>,
) -> Response {
    match buffer.span(&run_id, &span_id) {
        Some(record)
            if record
                .labels
                .get(SESSION_LABEL_KEY)
                .is_some_and(|value| value == &session_id) =>
        {
            Json::<SpanRecord>(record).into_response()
        }
        Some(_) | None => (
            StatusCode::NOT_FOUND,
            "span not found for session in buffer",
        )
            .into_response(),
    }
}

/// `GET /v1/tracing/usage` — buffer-wide token usage rollup.
///
/// Without a query parameter the response aggregates every record in the
/// buffer. `?label=key:value` filters to records whose correlation labels
/// contain the matching entry — useful for slicing by `agent_type`,
/// `turn`, or any other propagated label.
async fn usage_handler(
    State(state): State<UsageState>,
    Query(query): Query<UsageQuery>,
) -> Json<TokenUsageResponse> {
    let pricing = pricing_for_aggregate(&state.pricing);
    let response = match query.label {
        Some(filter) => state
            .buffer
            .aggregate_usage_by_label(&filter.key, &filter.value, pricing),
        None => state.buffer.aggregate_usage(pricing),
    };
    Json(response)
}

/// `GET /v1/tracing/runs/{run_id}/usage` — per-run usage rollup.
async fn run_usage_handler(
    State(state): State<UsageState>,
    Path(run_id): Path<String>,
) -> Response {
    let pricing = pricing_for_aggregate(&state.pricing);
    match state.buffer.aggregate_usage_for_run(&run_id, pricing) {
        Some(response) => Json::<TokenUsageResponse>(response).into_response(),
        None => (StatusCode::NOT_FOUND, "run not found in buffer").into_response(),
    }
}

/// `GET /v1/sessions/{session_id}/usage` — session-scoped usage rollup.
///
/// Sums tokens across every run for the session that's still in the
/// buffer. Returns a zeroed response (rather than 404) for unknown
/// sessions — "no LLM activity yet" is a valid steady state.
async fn sessions_usage_handler(
    State(state): State<UsageState>,
    Path(session_id): Path<String>,
) -> Json<TokenUsageResponse> {
    let pricing = pricing_for_aggregate(&state.pricing);
    Json(
        state
            .buffer
            .aggregate_usage_by_label(SESSION_LABEL_KEY, &session_id, pricing),
    )
}

/// `GET /v1/sessions/{session_id}/runs/{run_id}/usage` — per-run usage
/// rollup, gated on session membership.
async fn sessions_run_usage_handler(
    State(state): State<UsageState>,
    Path((session_id, run_id)): Path<(String, String)>,
) -> Response {
    let pricing = pricing_for_aggregate(&state.pricing);
    let Some(response) = state.buffer.aggregate_usage_for_run(&run_id, pricing) else {
        return (StatusCode::NOT_FOUND, "run not found in buffer").into_response();
    };
    // Membership gate: confirm at least one record under this run carries
    // the requested session label. We rely on the existing run-tree path
    // to surface labels, but for usage we re-check directly so we don't
    // pay for a full tree build.
    let belongs = state
        .buffer
        .distinct_runs_by_label(SESSION_LABEL_KEY, &session_id, usize::MAX)
        .into_iter()
        .any(|summary| summary.run_id == run_id);
    if !belongs {
        return (StatusCode::NOT_FOUND, "run not found for session in buffer").into_response();
    }
    Json::<TokenUsageResponse>(response).into_response()
}

/// Returns `Some(&pricing)` when the table has at least one rate
/// registered. Skipping the lookup entirely when empty keeps `cost_usd`
/// `None` end-to-end without paying per-record lock acquisition cost.
fn pricing_for_aggregate(pricing: &UsagePricing) -> Option<&UsagePricing> {
    if pricing.is_empty() {
        None
    } else {
        Some(pricing)
    }
}
