//! Session metadata injected into execution contexts.

use crate::store::SessionId;
use polaris_system::resource::LocalResource;

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
    pub turn_number: u32,
}

impl LocalResource for SessionInfo {}
