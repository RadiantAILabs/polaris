//! HTTP error response mapping for session endpoints.

use crate::error::SessionError;
use crate::store::{SessionId, TurnNumber};
use axum::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;

/// HTTP error type for session endpoints.
///
/// Maps [`SessionError`] variants to appropriate HTTP status codes and
/// JSON error bodies.
pub(crate) enum ApiError {
    /// Server not yet ready (state not initialized).
    NotReady,
    /// Session not found.
    SessionNotFound(SessionId),
    /// Session is already executing a turn.
    SessionBusy(SessionId),
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
    /// Internal server error.
    Internal(String),
}

impl ApiError {
    /// Returns the machine-readable error code for this variant.
    fn code(&self) -> &'static str {
        match self {
            Self::NotReady => "service_unavailable",
            Self::SessionNotFound(_) => "session_not_found",
            Self::SessionBusy(_) => "session_busy",
            Self::TurnNotFound(_) => "turn_not_found",
            Self::AgentNotFound(_) => "agent_not_found",
            Self::Conflict(_) => "session_already_exists",
            Self::GraphValidation(_) => "graph_validation",
            Self::IoChannelClosed => "io_channel_closed",
            Self::Internal(_) => "internal_error",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            Self::NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server not ready".to_owned(),
            ),
            Self::SessionNotFound(id) => {
                (StatusCode::NOT_FOUND, format!("session not found: {id}"))
            }
            Self::SessionBusy(id) => (
                StatusCode::CONFLICT,
                format!("session already executing a turn: {id}"),
            ),
            // 400 (not 404): the turn number is caller-supplied input that
            // failed validation, not a missing sub-resource.
            Self::TurnNotFound(turn) => (
                StatusCode::BAD_REQUEST,
                format!("no checkpoint for turn: {turn}"),
            ),
            Self::AgentNotFound(name) => {
                (StatusCode::BAD_REQUEST, format!("agent not found: {name}"))
            }
            Self::Conflict(id) => (
                StatusCode::CONFLICT,
                format!("session already exists: {id}"),
            ),
            Self::GraphValidation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg.clone()),
            Self::IoChannelClosed => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "IO channel closed unexpectedly".to_owned(),
            ),
            Self::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };
        let code = self.code();
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
