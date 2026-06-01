//! Session metadata types.

use crate::store::{AgentTypeId, SessionId, TurnNumber};
use polaris_system::resource::LocalResource;
use serde::Serialize;
#[cfg(feature = "typegen")]
use ts_rs::TS;

/// Metadata about the current session, injected into the context
/// at the start of each [`process_turn`](crate::api::SessionsAPI::process_turn).
///
/// Systems can read this via `Res<SessionInfo>` to access the current
/// session ID and turn number.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The session's unique identifier.
    pub session_id: SessionId,
    /// The current turn number (starts at 0, incremented after each turn).
    pub turn_number: TurnNumber,
}

impl LocalResource for SessionInfo {}

/// Status of a live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionStatus {
    /// The session is active and ready to process turns.
    Active,
    /// The session is preserved but cannot accept further turns.
    ///
    /// Set when a session is finalized by
    /// [`SessionsAPI::run_oneshot_preserved`](crate::SessionsAPI::run_oneshot_preserved).
    /// Query surfaces (turn history, metadata, persistence) remain
    /// available; any method that would mutate session state returns
    /// [`SessionError::ReadOnly`](crate::SessionError::ReadOnly).
    ReadOnly,
}

/// Metadata about a session returned by
/// [`SessionsAPI::session_info`](crate::api::SessionsAPI::session_info).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct SessionMetadata {
    /// The session's unique identifier.
    pub session_id: SessionId,
    /// The type of the agent that owns this session.
    pub agent_type: AgentTypeId,
    /// The current turn number.
    #[cfg_attr(feature = "typegen", ts(type = "number"))]
    pub turn_number: TurnNumber,
    /// ISO 8601 UTC timestamp of when the session was created.
    pub created_at: String,
    /// The session's current status.
    pub status: SessionStatus,
}
