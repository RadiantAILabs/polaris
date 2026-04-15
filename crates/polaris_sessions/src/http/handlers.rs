//! Axum handler functions for session HTTP endpoints.

use super::DeferredState;
use super::error::ApiError;
use super::models::{
    CheckpointResponse, CreateSessionRequest, CreateSessionResponse, ListCheckpointsResponse,
    ListSessionsResponse, ListStoredSessionsResponse, ProcessTurnRequest, ProcessTurnResponse,
    RollbackRequest, TurnExecutionMetadata,
};
use crate::api::SessionsAPI;
use crate::info::SessionMetadata;
use crate::store::SessionId;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use polaris_app::{HttpHeaders, HttpIOProvider};
use polaris_core_plugins::{IOMessage, UserIO};
use std::sync::Arc;

/// Extracts the initialized state, or returns 503 if not ready.
fn get_sessions(deferred: &DeferredState) -> Result<&SessionsAPI, ApiError> {
    deferred.get().ok_or(ApiError::NotReady)
}

/// `POST /v1/sessions` — create a new session.
pub(crate) async fn create_session(
    State(deferred): State<DeferredState>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let sessions = get_sessions(&deferred)?;

    let agent_type = sessions
        .find_agent_type(&body.agent_type)
        .ok_or_else(|| ApiError::AgentNotFound(body.agent_type.clone()))?;

    let session_id = body
        .session_id
        .map(SessionId::from_string)
        .unwrap_or_default();

    let ctx = sessions.create_context();
    sessions.create_session(ctx, &session_id, &agent_type)?;

    let info = sessions.session_info(&session_id)?;
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id: info.session_id.as_str().to_owned(),
            agent_type: info.agent_type.as_str().to_owned(),
            turn_number: info.turn_number,
            created_at: info.created_at,
            status: info.status,
        }),
    ))
}

/// `GET /v1/sessions` — list all live sessions.
pub(crate) async fn list_sessions(
    State(deferred): State<DeferredState>,
) -> Result<Json<ListSessionsResponse>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let list = sessions.list_session_metadata();
    Ok(Json(ListSessionsResponse { sessions: list }))
}

/// `GET /v1/sessions/{id}` — get session info.
pub(crate) async fn get_session(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<Json<SessionMetadata>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    let info = sessions.session_info(&session_id)?;
    Ok(Json(info))
}

/// `DELETE /v1/sessions/{id}` — delete a session.
pub(crate) async fn delete_session(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    sessions.delete_session(&session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/sessions/{id}/turns` — process a turn.
///
/// Creates an [`HttpIOProvider`] to bridge the request message to the
/// agent's [`UserIO`] and collects all agent output into the response.
pub(crate) async fn process_turn(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ProcessTurnRequest>,
) -> Result<Json<ProcessTurnResponse>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);

    // Set up IO channels. Input is bounded (single user message per turn);
    // output is unbounded so agent output never blocks during execution.
    let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1);
    let provider = Arc::new(provider);

    // Send user message and close the input channel.
    input_tx
        .send(IOMessage::user_text(body.message))
        .await
        .map_err(|_| ApiError::IoChannelClosed)?;
    drop(input_tx);

    // Execute the turn, injecting the IO provider and raw request headers.
    // `RequestContextPlugin`'s `OnGraphStart` hook parses `HttpHeaders` into
    // a `RequestContext` before any system runs.
    let io_provider = Arc::clone(&provider);
    let result = sessions
        .try_process_turn_with(&session_id, move |ctx| {
            ctx.insert(UserIO::new(io_provider));
            ctx.insert(HttpHeaders(headers));
        })
        .await?;

    // Drain all output messages (already buffered after execution).
    let mut messages = Vec::new();
    while let Ok(msg) = output_rx.try_recv() {
        messages.push(msg);
    }

    let info = sessions.session_info(&session_id)?;

    Ok(Json(ProcessTurnResponse {
        messages,
        execution: TurnExecutionMetadata {
            nodes_executed: result.nodes_executed,
            duration_ms: result.duration.as_millis() as u64,
            turn_number: info.turn_number,
        },
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoints
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /v1/sessions/{id}/checkpoints` — create a checkpoint.
pub(crate) async fn create_checkpoint(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<CheckpointResponse>), ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    let turn_number = sessions.checkpoint(&session_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(CheckpointResponse { turn_number }),
    ))
}

/// `GET /v1/sessions/{id}/checkpoints` — list checkpoints.
pub(crate) async fn list_checkpoints(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<Json<ListCheckpointsResponse>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    let turns = sessions.list_checkpoints(&session_id)?;
    let checkpoints = turns
        .into_iter()
        .map(|turn_number| CheckpointResponse { turn_number })
        .collect();
    Ok(Json(ListCheckpointsResponse { checkpoints }))
}

/// `POST /v1/sessions/{id}/rollback` — rollback to a checkpoint.
pub(crate) async fn rollback(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
    Json(body): Json<RollbackRequest>,
) -> Result<Json<SessionMetadata>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    sessions.rollback(&session_id, body.turn_number).await?;
    let info = sessions.session_info(&session_id)?;
    Ok(Json(info))
}

// ─────────────────────────────────────────────────────────────────────────────
// Persistence (store)
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /v1/sessions/{id}/save` — persist session to the backing store.
pub(crate) async fn save_session(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    sessions.save_session(&session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/sessions/{id}/resume` — load session from the backing store.
///
/// Creates a fresh execution context, deserializes persisted resources,
/// and registers the session as a live in-memory session.
pub(crate) async fn resume_session(
    State(deferred): State<DeferredState>,
    Path(id): Path<String>,
) -> Result<Json<SessionMetadata>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let session_id = SessionId::from_string(id);
    let ctx = sessions.create_context();
    sessions.resume_session(ctx, &session_id).await?;
    let info = sessions.session_info(&session_id)?;
    Ok(Json(info))
}

/// `GET /v1/sessions/stored` — list sessions in the backing store.
pub(crate) async fn list_stored_sessions(
    State(deferred): State<DeferredState>,
) -> Result<Json<ListStoredSessionsResponse>, ApiError> {
    let sessions = get_sessions(&deferred)?;
    let ids = sessions.list_sessions().await?;
    let sessions_list = ids.iter().map(|id| id.as_str().to_owned()).collect();
    Ok(Json(ListStoredSessionsResponse {
        sessions: sessions_list,
    }))
}
