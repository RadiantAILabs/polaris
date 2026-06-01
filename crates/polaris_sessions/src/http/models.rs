//! Request and response types for session HTTP endpoints.

use crate::info::{SessionMetadata, SessionStatus};
use crate::store::TurnNumber;
use polaris_core_plugins::IOMessage;
use serde::{Deserialize, Serialize};
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Request body for `POST /v1/sessions`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional session ID. A random ID is generated if omitted.
    pub session_id: Option<String>,
    /// The registered agent type name.
    pub agent_type: String,
}

/// Response body for `POST /v1/sessions`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreateSessionResponse {
    /// The session's unique identifier.
    pub session_id: String,
    /// The agent type name.
    pub agent_type: String,
    /// The current turn number (0 for newly created sessions).
    pub turn_number: TurnNumber,
    /// ISO 8601 UTC timestamp of when the session was created.
    pub created_at: String,
    /// The session's current status.
    pub status: SessionStatus,
}

/// Response body for `GET /v1/sessions`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListSessionsResponse {
    /// All live sessions.
    pub sessions: Vec<SessionMetadata>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Turn processing
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for `POST /v1/sessions/{id}/turns`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProcessTurnRequest {
    /// The user message to send to the agent.
    pub message: String,
}

/// Response body for `POST /v1/sessions/{id}/turns`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
pub struct ProcessTurnResponse {
    /// Output messages produced by the agent during this turn.
    pub messages: Vec<IOMessage>,
    /// Execution metadata for the completed turn.
    pub execution: TurnExecutionMetadata,
}

/// Execution metadata for a completed turn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnExecutionMetadata {
    /// Number of graph nodes executed during this turn.
    pub nodes_executed: usize,
    /// Turn execution duration in milliseconds.
    pub duration_ms: u64,
    /// The session's turn number after this turn completed.
    pub turn_number: TurnNumber,
}

/// Terminal SSE event for `POST /v1/sessions/{id}/turns/stream`.
///
/// Sent as an `event: done` SSE event when the turn completes
/// successfully. Contains the same execution metadata as the
/// synchronous [`ProcessTurnResponse`].
///
/// # JSON representation
///
/// ```json
/// {
///     "execution": {
///         "nodes_executed": 3,
///         "duration_ms": 142,
///         "turn_number": 1
///     }
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamTurnDone {
    /// Execution metadata for the completed turn.
    pub execution: TurnExecutionMetadata,
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoints
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata for a single checkpoint.
///
/// Returned by `POST /v1/sessions/{id}/checkpoints` (create) and as
/// an entry in [`ListCheckpointsResponse`].
///
/// # JSON representation
///
/// ```json
/// {
///     "turn_number": 3
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointResponse {
    /// The turn number at which the checkpoint was taken.
    pub turn_number: TurnNumber,
}

/// Response body for `GET /v1/sessions/{id}/checkpoints`.
///
/// # JSON representation
///
/// ```json
/// {
///     "checkpoints": [
///         { "turn_number": 1 },
///         { "turn_number": 3 },
///         { "turn_number": 5 }
///     ]
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListCheckpointsResponse {
    /// All checkpoints for the session, ordered oldest first.
    pub checkpoints: Vec<CheckpointResponse>,
}

/// Request body for `POST /v1/sessions/{id}/rollback`.
///
/// # JSON representation
///
/// ```json
/// {
///     "turn_number": 3
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RollbackRequest {
    /// The turn number to roll back to.
    pub turn_number: TurnNumber,
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistence (store)
// ─────────────────────────────────────────────────────────────────────────────

/// Response body for `GET /v1/sessions/stored`.
///
/// # JSON representation
///
/// ```json
/// {
///     "sessions": ["abc123", "def456"]
/// }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListStoredSessionsResponse {
    /// Session IDs known to the backing store.
    pub sessions: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Agent types (A9 — dashboard)
// ─────────────────────────────────────────────────────────────────────────────

/// Summary of a registered agent type, returned by
/// `GET /v1/sessions/agent-types`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct AgentTypeSummary {
    /// The agent's stable type name.
    pub name: String,
}

/// Response body for `GET /v1/sessions/agent-types`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct ListAgentTypesResponse {
    /// All registered agent types.
    pub items: Vec<AgentTypeSummary>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Turn history (A9 — dashboard)
// ─────────────────────────────────────────────────────────────────────────────

/// Status of a single turn within a session's history.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// The turn is still executing.
    Running,
    /// The turn finished successfully.
    Completed,
    /// The turn failed with an error.
    Failed,
}

/// Summary entry for `GET /v1/sessions/{id}/turns`.
///
/// When the request includes `?include=messages`, the full
/// [`IOMessage`] array is embedded in [`messages`](Self::messages).
/// Otherwise that field is omitted from the JSON payload.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct TurnSummary {
    /// The turn number.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub turn: TurnNumber,
    /// ISO 8601 UTC timestamp when the turn began.
    pub started_at: String,
    /// ISO 8601 UTC timestamp when the turn finished (None if still running).
    pub finished_at: Option<String>,
    /// Outcome of the turn.
    pub status: TurnStatus,
    /// Number of IO messages emitted during the turn.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub io_message_count: u32,
    /// Truncated text of the most recent IO message, if any.
    pub last_message_preview: Option<String>,
    /// Full IO messages, only present when the request was made with
    /// `?include=messages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typegen", ts(optional, type = "unknown[]"))]
    pub messages: Option<Vec<IOMessage>>,
}

/// Response body for `GET /v1/sessions/{id}/turns`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct ListTurnsResponse {
    /// Recorded turns for the session, oldest first.
    pub items: Vec<TurnSummary>,
}

/// Full turn payload returned by `GET /v1/sessions/{id}/turns/{n}`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct Turn {
    /// The turn number.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub turn: TurnNumber,
    /// ISO 8601 UTC timestamp when the turn began.
    pub started_at: String,
    /// ISO 8601 UTC timestamp when the turn finished (None if still running).
    pub finished_at: Option<String>,
    /// Outcome of the turn.
    pub status: TurnStatus,
    /// IO messages emitted during the turn, in arrival order.
    #[cfg_attr(feature = "typegen", ts(type = "unknown[]"))]
    pub messages: Vec<IOMessage>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Uptime (A9 — dashboard)
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed bucket granularity for the uptime endpoint's `?bucket=` query.
///
/// The wire form uses the short string codes (`1m`, `5m`, `15m`, `1h`)
/// rather than the variant names; unknown values are rejected at
/// deserialization time so misclients cannot ask for arbitrary buckets.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub enum BucketGranularity {
    /// One-minute buckets.
    #[serde(rename = "1m")]
    OneMinute,
    /// Five-minute buckets.
    #[serde(rename = "5m")]
    FiveMinutes,
    /// Fifteen-minute buckets.
    #[serde(rename = "15m")]
    FifteenMinutes,
    /// One-hour buckets.
    #[serde(rename = "1h")]
    OneHour,
}

/// Lifecycle status reported for a single uptime bucket.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum UptimeStatus {
    /// No lifecycle events observed in or before this bucket.
    Unknown,
    /// Session exists but is not currently processing a turn.
    Idle,
    /// Session is actively processing a turn.
    Active,
    /// Session has been deleted.
    Terminated,
}

/// A single bucket in an uptime time-series.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct SessionUptimeBucket {
    /// ISO 8601 UTC start of the bucket window (inclusive).
    pub start: String,
    /// ISO 8601 UTC end of the bucket window (exclusive).
    pub end: String,
    /// Dominant status during the bucket window.
    pub status: UptimeStatus,
}

/// Response body for `GET /v1/sessions/{id}/uptime`.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct SessionUptimeResponse {
    /// The bucket granularity that was used.
    pub bucket: BucketGranularity,
    /// ISO 8601 UTC start of the queried range (inclusive).
    pub since: String,
    /// ISO 8601 UTC end of the queried range (exclusive).
    pub until: String,
    /// Buckets in chronological order; length is
    /// `(until − since) / bucket`.
    pub buckets: Vec<SessionUptimeBucket>,
}
