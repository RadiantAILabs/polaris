//! Axum handler functions for session HTTP endpoints.

use super::error::{ApiError, codes};
use super::models::{
    AgentTypeSummary, BucketGranularity, CheckpointResponse, CreateSessionRequest,
    CreateSessionResponse, ListAgentTypesResponse, ListCheckpointsResponse, ListSessionsResponse,
    ListStoredSessionsResponse, ListTurnsResponse, ProcessTurnRequest, ProcessTurnResponse,
    RollbackRequest, SessionUptimeResponse, StreamTurnDone, Turn, TurnExecutionMetadata,
};
use crate::api::SessionsAPI;
use crate::http::HttpIOProvider;
use crate::info::SessionMetadata;
use crate::store::{SessionId, TurnNumber};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use parking_lot::Mutex;
use polaris_app::HttpHeaders;
use polaris_core_plugins::{IOError, IOMessage, IOProvider, IOSource, UserIO};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

/// `POST /v1/sessions` — create a new session.
pub(crate) async fn create_session(
    State(sessions): State<SessionsAPI>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
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
    State(sessions): State<SessionsAPI>,
) -> Result<Json<ListSessionsResponse>, ApiError> {
    let list = sessions.list_session_metadata();
    Ok(Json(ListSessionsResponse { sessions: list }))
}

/// `GET /v1/sessions/{id}` — get session info.
pub(crate) async fn get_session(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<Json<SessionMetadata>, ApiError> {
    let session_id = SessionId::from_string(id);
    let info = sessions.session_info(&session_id)?;
    Ok(Json(info))
}

/// `DELETE /v1/sessions/{id}` — delete a session.
pub(crate) async fn delete_session(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session_id = SessionId::from_string(id);
    sessions.delete_session(&session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Default per-turn output channel capacity.
///
/// Picked to absorb short bursts of agent output without blocking while
/// still bounding memory use if the response consumer lags. Tuned together
/// for [`process_turn`] (drained once after the turn completes) and
/// [`process_turn_stream`] (drained continuously as messages arrive).
const TURN_OUTPUT_BUFFER: usize = 64;

/// `POST /v1/sessions/{id}/turns` — process a turn.
///
/// Creates an [`HttpIOProvider`] to bridge the request message to the
/// agent's [`UserIO`] and collects all agent output into the response.
///
/// Both input and output channels are bounded; agents that emit more than
/// [`TURN_OUTPUT_BUFFER`] messages within a turn block on `send()` until
/// the receiver catches up. Since this handler only drains after the turn
/// finishes, that effectively caps a single turn's buffered output.
pub(crate) async fn process_turn(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ProcessTurnRequest>,
) -> Result<Json<ProcessTurnResponse>, ApiError> {
    let session_id = SessionId::from_string(id);

    // Snapshot the turn number before execution so we can key the
    // post-turn message recording on the same value `execute_turn` used
    // for its history entry (the session's atomic counter is incremented
    // on success, so reading it after the call would be off-by-one).
    let turn_before = sessions.session_info(&session_id)?.turn_number;

    // Bounded input (one user message per turn) and bounded output to
    // apply backpressure on agents that emit faster than we drain.
    let (provider, input_tx, mut output_rx) = HttpIOProvider::new(1, TURN_OUTPUT_BUFFER);
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

    sessions.record_turn_messages(&session_id, turn_before, messages.clone());

    let info = sessions.session_info(&session_id)?;

    Ok(Json(ProcessTurnResponse {
        messages,
        execution: TurnExecutionMetadata {
            nodes_executed: result.nodes_executed(),
            duration_ms: result.duration().as_millis() as u64,
            turn_number: info.turn_number,
        },
    }))
}

/// Keep-alive interval for SSE streams.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Returns the SSE event type string for an [`IOSource`] variant.
fn sse_event_name(source: &IOSource) -> &'static str {
    match source {
        IOSource::User => "user",
        IOSource::Agent(_) => "agent",
        IOSource::External(_) => "external",
        IOSource::System => "system",
    }
}

/// Builds a structured `event: error` SSE event with `{ code, message }` JSON.
///
/// `serde_json::json!` escapes `message` correctly even when it contains
/// quotes or newlines from upstream error formatting, so the payload is
/// always valid JSON.
fn error_event(code: &str, message: &str) -> Event {
    Event::default()
        .event("error")
        .json_data(serde_json::json!({ "code": code, "message": message }))
        .unwrap_or_else(|_| {
            // Plain strings always serialize via `serde_json`; this branch is
            // defensive only.
            Event::default().event("error").data("internal error")
        })
}

/// `POST /v1/sessions/{id}/turns/stream` — process a turn with SSE streaming.
///
/// Streaming alternative to [`process_turn`]. Instead of buffering all
/// agent output and returning it in a JSON response, this endpoint
/// streams each [`IOMessage`] as an SSE event as the agent emits it.
///
/// The stream ends with a terminal event:
/// - `event: done` with [`StreamTurnDone`] on success
/// - `event: error` with `{ code, message }` on failure
///
/// # Cancellation and lifecycle
///
/// The turn runs on a detached `tokio::spawn` so the SSE response can begin
/// streaming before the turn completes. The handler does **not** abort the
/// background task on client disconnect — turns are typically driven by
/// LLM calls that cannot be safely interrupted mid-flight. Instead,
/// disconnects propagate via backpressure:
///
/// * dropping the SSE response drops `output_rx`, which closes the bounded
///   output channel from the receiver side;
/// * the next `provider.send(..)` from the agent then returns
///   [`IOError::Closed`](polaris_core_plugins::IOError::Closed), which
///   well-behaved systems propagate as an error;
/// * the turn finishes (with that error or by completing naturally),
///   `provider.close()` runs, and the spawn exits.
///
/// In short: a disconnected client cannot leak an unbounded amount of
/// memory, but it may continue to consume CPU until the in-flight LLM
/// call returns.
pub(crate) async fn process_turn_stream(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ProcessTurnRequest>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let session_id = SessionId::from_string(id);

    // Pre-stream validation: fail with a normal HTTP error before
    // committing to the SSE response.
    let info = sessions.session_info(&session_id)?;
    let turn_before = info.turn_number;

    let (provider, input_tx, output_rx) = HttpIOProvider::new(1, TURN_OUTPUT_BUFFER);
    let provider = Arc::new(provider);
    // Wrap with a recorder so the spawned task can flush the full
    // message array to the session's turn history once the turn ends.
    // The wrapper records on `send`, before the bounded channel is
    // touched, so it captures every message even if the SSE consumer
    // is still draining `output_rx` when we finalize the record.
    let recorded: Arc<Mutex<Vec<IOMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let recording = Arc::new(RecordingIOProvider {
        inner: Arc::clone(&provider),
        recorded: Arc::clone(&recorded),
    });

    input_tx
        .send(IOMessage::user_text(body.message))
        .await
        .map_err(|_| ApiError::IoChannelClosed)?;
    drop(input_tx);

    // Channel for the terminal SSE event (done or error). Capacity 1 —
    // exactly one terminal event is ever sent.
    let (term_tx, term_rx) = tokio::sync::mpsc::channel::<Event>(1);

    // Spawn turn execution in the background so the SSE response
    // can start streaming before the turn completes. See the
    // `# Cancellation and lifecycle` section above for why this is
    // detached rather than aborted on client disconnect.
    let sessions_bg = sessions.clone();
    let session_id_bg = session_id.clone();
    let recording_bg = Arc::clone(&recording);
    let recorded_bg = Arc::clone(&recorded);
    tokio::spawn(async move {
        let result = sessions_bg
            .try_process_turn_with(&session_id_bg, move |ctx| {
                ctx.insert(UserIO::new(recording_bg));
                ctx.insert(HttpHeaders(headers));
            })
            .await;

        // Close the output channel so the IOMessage stream terminates.
        provider.close().await;

        // Flush captured messages to the turn history. Cloning is fine —
        // a turn's IO volume is bounded by the agent's behavior, not by
        // the channel buffer.
        let captured = recorded_bg.lock().clone();
        sessions_bg.record_turn_messages(&session_id_bg, turn_before, captured);

        // Send terminal event.
        let event = match result {
            Ok(exec_result) => {
                let turn_number = sessions_bg
                    .session_info(&session_id_bg)
                    .map(|info| info.turn_number)
                    .unwrap_or(0);
                Event::default()
                    .event("done")
                    .json_data(&StreamTurnDone {
                        execution: TurnExecutionMetadata {
                            nodes_executed: exec_result.nodes_executed(),
                            duration_ms: exec_result.duration().as_millis() as u64,
                            turn_number,
                        },
                    })
                    .unwrap_or_else(|json_err| {
                        error_event(
                            codes::INTERNAL_ERROR,
                            &format!("serialization failed: {json_err}"),
                        )
                    })
            }
            Err(session_err) => {
                let api_err = ApiError::from(session_err);
                error_event(api_err.code(), &api_err.message())
            }
        };
        let _ = term_tx.send(event).await;
    });

    // IOMessage stream → SSE events.
    let io_stream = ReceiverStream::new(output_rx).map(|msg| {
        let name = sse_event_name(&msg.source);
        Ok::<_, Infallible>(Event::default().event(name).json_data(&msg).unwrap_or_else(
            |json_err| {
                error_event(
                    codes::INTERNAL_ERROR,
                    &format!("serialization failed: {json_err}"),
                )
            },
        ))
    });

    // Terminal event stream (yields one event then closes).
    let term_stream = ReceiverStream::new(term_rx).map(Ok::<_, Infallible>);

    let combined = io_stream.chain(term_stream);

    Ok(Sse::new(combined).keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoints
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /v1/sessions/{id}/checkpoints` — create a checkpoint.
pub(crate) async fn create_checkpoint(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<CheckpointResponse>), ApiError> {
    let session_id = SessionId::from_string(id);
    let turn_number = sessions.checkpoint(&session_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(CheckpointResponse { turn_number }),
    ))
}

/// `GET /v1/sessions/{id}/checkpoints` — list checkpoints.
pub(crate) async fn list_checkpoints(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<Json<ListCheckpointsResponse>, ApiError> {
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
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
    Json(body): Json<RollbackRequest>,
) -> Result<Json<SessionMetadata>, ApiError> {
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
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session_id = SessionId::from_string(id);
    sessions.save_session(&session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/sessions/{id}/resume` — load session from the backing store.
///
/// Creates a fresh execution context, deserializes persisted resources,
/// and registers the session as a live in-memory session.
pub(crate) async fn resume_session(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
) -> Result<Json<SessionMetadata>, ApiError> {
    let session_id = SessionId::from_string(id);
    let ctx = sessions.create_context();
    sessions.resume_session(ctx, &session_id).await?;
    let info = sessions.session_info(&session_id)?;
    Ok(Json(info))
}

/// `GET /v1/sessions/stored` — list sessions in the backing store.
pub(crate) async fn list_stored_sessions(
    State(sessions): State<SessionsAPI>,
) -> Result<Json<ListStoredSessionsResponse>, ApiError> {
    let ids = sessions.list_sessions().await?;
    let sessions_list = ids.iter().map(|id| id.as_str().to_owned()).collect();
    Ok(Json(ListStoredSessionsResponse {
        sessions: sessions_list,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Dashboard endpoints (A9)
// ─────────────────────────────────────────────────────────────────────────────

/// `GET /v1/sessions/agent-types` — enumerate registered agent types.
pub(crate) async fn list_agent_types(
    State(sessions): State<SessionsAPI>,
) -> Json<ListAgentTypesResponse> {
    let items = sessions
        .registered_agents()
        .into_iter()
        .map(|name| AgentTypeSummary {
            name: name.to_owned(),
        })
        .collect();
    Json(ListAgentTypesResponse { items })
}

/// Query parameters for `GET /v1/sessions/{id}/turns`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TurnsListQuery {
    /// Comma-separated list of optional sections to include. Currently
    /// the only recognized token is `messages` — when present, each
    /// summary embeds the full IO message array.
    include: Option<String>,
}

impl TurnsListQuery {
    fn include_messages(&self) -> bool {
        self.include
            .as_deref()
            .map(|s| s.split(',').any(|token| token.trim() == "messages"))
            .unwrap_or(false)
    }
}

/// `GET /v1/sessions/{id}/turns` — list recorded turn summaries.
pub(crate) async fn list_turns(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
    Query(query): Query<TurnsListQuery>,
) -> Result<Json<ListTurnsResponse>, ApiError> {
    let session_id = SessionId::from_string(id);
    let items = sessions.turn_history(&session_id, query.include_messages())?;
    Ok(Json(ListTurnsResponse { items }))
}

/// `GET /v1/sessions/{id}/turns/{n}` — fetch a single turn's full payload.
pub(crate) async fn get_turn(
    State(sessions): State<SessionsAPI>,
    Path((id, turn)): Path<(String, TurnNumber)>,
) -> Result<Json<Turn>, ApiError> {
    let session_id = SessionId::from_string(id);
    let turn = sessions.turn(&session_id, turn)?;
    Ok(Json(turn))
}

/// Query parameters for `GET /v1/sessions/{id}/uptime`.
///
/// `bucket` defaults to `1m` when omitted. Unknown values fail
/// deserialization and surface as a 400 from axum's `Query` extractor,
/// matching the spec's "reject unknown buckets" contract.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UptimeQuery {
    bucket: Option<BucketGranularity>,
    since: Option<String>,
    until: Option<String>,
}

/// Default lookback window for uptime queries when `?since=` is omitted.
const DEFAULT_UPTIME_LOOKBACK_HOURS: i64 = 24;

/// Upper bound on the number of uptime buckets a single request may produce.
///
/// `since`, `until`, and `bucket` are all client-controlled. Without this cap
/// a request for a wide range at `1m` granularity drives an unbounded
/// `Vec::with_capacity` in the recorder and can exhaust server memory.
/// 10 000 one-minute buckets is ~7 days — well past any real dashboard
/// window — so a larger range is rejected as a bad request.
const MAX_UPTIME_BUCKETS: usize = 10_000;

fn parse_iso8601(field: &str, value: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| ApiError::BadRequest(format!("invalid `{field}` timestamp: {err}")))
}

/// `GET /v1/sessions/{id}/uptime` — bucketed lifecycle time-series.
pub(crate) async fn get_uptime(
    State(sessions): State<SessionsAPI>,
    Path(id): Path<String>,
    Query(query): Query<UptimeQuery>,
) -> Result<Json<SessionUptimeResponse>, ApiError> {
    let session_id = SessionId::from_string(id);

    let bucket = query.bucket.unwrap_or(BucketGranularity::OneMinute);
    let until = match query.until.as_deref() {
        Some(value) => parse_iso8601("until", value)?,
        None => Utc::now(),
    };
    let since = match query.since.as_deref() {
        Some(value) => parse_iso8601("since", value)?,
        None => until - ChronoDuration::hours(DEFAULT_UPTIME_LOOKBACK_HOURS),
    };

    // Reject ranges that would allocate an unbounded bucket vector before
    // any work is done. `since`/`until`/`bucket` are all client-controlled.
    let num_buckets = crate::uptime::bucket_count(bucket, since, until);
    if num_buckets > MAX_UPTIME_BUCKETS {
        return Err(ApiError::BadRequest(format!(
            "requested range yields {num_buckets} buckets, exceeding the limit \
             of {MAX_UPTIME_BUCKETS}; widen `bucket` or narrow the \
             `since`/`until` range"
        )));
    }

    let response = sessions.uptime_buckets(&session_id, bucket, since, until)?;
    Ok(Json(response))
}

// ─────────────────────────────────────────────────────────────────────────────
// Turn-message recording (shared between sync + SSE turn handlers)
// ─────────────────────────────────────────────────────────────────────────────

/// IO provider that mirrors every outbound message into a shared `Vec`
/// before delegating to an inner [`HttpIOProvider`].
///
/// Used by [`process_turn_stream`] so the SSE handler can attach the full
/// IO message array to the session's turn history *after* the turn ends,
/// without the synchronous handler's "drain `output_rx`" trick (the SSE
/// stream is consumed by Axum's response loop, not the handler body).
struct RecordingIOProvider {
    inner: Arc<HttpIOProvider>,
    recorded: Arc<Mutex<Vec<IOMessage>>>,
}

impl IOProvider for RecordingIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.recorded.lock().push(message.clone());
        self.inner.send(message).await
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.inner.receive().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}
