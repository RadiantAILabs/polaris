//! HTTP error response mapping for session endpoints.

use crate::error::SessionError;
use crate::store::{SessionId, TurnNumber};
use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;

/// Machine-readable error code strings emitted in the `code` field of
/// JSON error responses and `event: error` SSE payloads.
///
/// These are part of the HTTP contract — adding, renaming, or removing
/// a code is a breaking change for clients. Keep this module the single
/// source of truth so handlers and [`ApiError::code`] cannot drift.
pub(crate) mod codes {
    pub(crate) const SESSION_NOT_FOUND: &str = "session_not_found";
    pub(crate) const SESSION_BUSY: &str = "session_busy";
    pub(crate) const SESSION_READ_ONLY: &str = "session_read_only";
    pub(crate) const TURN_NOT_FOUND: &str = "turn_not_found";
    pub(crate) const AGENT_NOT_FOUND: &str = "agent_not_found";
    pub(crate) const SESSION_ALREADY_EXISTS: &str = "session_already_exists";
    pub(crate) const GRAPH_VALIDATION: &str = "graph_validation";
    pub(crate) const IO_CHANNEL_CLOSED: &str = "io_channel_closed";
    pub(crate) const INTERNAL_ERROR: &str = "internal_error";
    pub(crate) const BAD_REQUEST: &str = "bad_request";
}

/// HTTP error type for session endpoints.
///
/// Maps [`SessionError`] variants to appropriate HTTP status codes and
/// JSON error bodies.
pub(crate) enum ApiError {
    /// Session not found.
    SessionNotFound(SessionId),
    /// Session is already executing a turn.
    SessionBusy(SessionId),
    /// Session is read-only and rejects mutation.
    SessionReadOnly(SessionId),
    /// No checkpoint exists for the given turn number.
    TurnNotFound(TurnNumber),
    /// Agent type not registered.
    AgentNotFound(String),
    /// A session with the given ID already exists.
    Conflict(SessionId),
    /// Agent graph failed validation (client configuration error).
    GraphValidation(String),
    /// IO channel was closed before the message could be sent.
    IoChannelClosed,
    /// Caller supplied an invalid query parameter or body field.
    BadRequest(String),
    /// Internal server error. Detail is logged via `tracing::error!` and
    /// is **not** included in the response body — clients receive a
    /// generic message so storage paths, host metadata, and other
    /// internals do not leak.
    Internal(String),
}

impl ApiError {
    /// Returns the machine-readable error code for this variant.
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::SessionNotFound(_) => codes::SESSION_NOT_FOUND,
            Self::SessionBusy(_) => codes::SESSION_BUSY,
            Self::SessionReadOnly(_) => codes::SESSION_READ_ONLY,
            Self::TurnNotFound(_) => codes::TURN_NOT_FOUND,
            Self::AgentNotFound(_) => codes::AGENT_NOT_FOUND,
            Self::Conflict(_) => codes::SESSION_ALREADY_EXISTS,
            Self::GraphValidation(_) => codes::GRAPH_VALIDATION,
            Self::IoChannelClosed => codes::IO_CHANNEL_CLOSED,
            Self::BadRequest(_) => codes::BAD_REQUEST,
            Self::Internal(_) => codes::INTERNAL_ERROR,
        }
    }

    /// Returns a human-readable error message for this variant.
    pub(super) fn message(&self) -> String {
        match self {
            Self::SessionNotFound(id) => format!("session not found: {id}"),
            Self::SessionBusy(id) => format!("session already executing a turn: {id}"),
            Self::SessionReadOnly(id) => format!("session is read-only: {id}"),
            Self::TurnNotFound(turn) => format!("no record for turn: {turn}"),
            Self::AgentNotFound(name) => format!("agent not found: {name}"),
            Self::Conflict(id) => format!("session already exists: {id}"),
            Self::GraphValidation(msg) | Self::BadRequest(msg) => msg.clone(),
            Self::IoChannelClosed => "IO channel closed unexpectedly".to_owned(),
            // Internal errors deliberately do not surface server-side
            // detail (file paths, error chains) to HTTP clients; the
            // full error is logged in [`IntoResponse`].
            Self::Internal(_) => "internal server error".to_owned(),
        }
    }

    /// Returns the HTTP status code for this variant.
    fn status(&self) -> StatusCode {
        match self {
            Self::SessionNotFound(_) => StatusCode::NOT_FOUND,
            Self::SessionBusy(_) | Self::Conflict(_) | Self::SessionReadOnly(_) => {
                StatusCode::CONFLICT
            }
            // 400 (not 404): the turn number is caller-supplied input that
            // failed validation, not a missing sub-resource.
            Self::TurnNotFound(_) | Self::AgentNotFound(_) | Self::BadRequest(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::GraphValidation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::IoChannelClosed | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        if let Self::Internal(detail) = &self {
            tracing::error!(
                error = %detail,
                "sessions HTTP: internal error returned to client"
            );
        }
        let status = self.status();
        let code = self.code();
        let message = self.message();
        (
            status,
            Json(serde_json::json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

impl From<SessionError> for ApiError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::SessionNotFound(id) => Self::SessionNotFound(id),
            SessionError::SessionBusy(id) => Self::SessionBusy(id),
            SessionError::ReadOnly(id) => Self::SessionReadOnly(id),
            SessionError::SessionAlreadyExists(id) => Self::Conflict(id),
            SessionError::TurnNotFound(turn) => Self::TurnNotFound(turn),
            SessionError::AgentNotFound(name) => Self::AgentNotFound(name),
            SessionError::GraphValidation {
                agent_name, result, ..
            } => Self::GraphValidation(format!(
                "graph validation failed for agent '{agent_name}': {result}"
            )),
            other => Self::Internal(other.to_string()),
        }
    }
}
