//! Request and response types for session HTTP endpoints.

use crate::info::{SessionMetadata, SessionStatus};
use crate::store::TurnNumber;
use polaris_core_plugins::IOMessage;
use serde::{Deserialize, Serialize};

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
