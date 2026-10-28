//! Session storage traits and types.
//!
//! This module defines the [`SessionStore`] trait for persisting session data,
//! along with the core identity and data types used across the sessions crate.

pub mod memory;

#[cfg(feature = "file-store")]
mod bounded;
#[cfg(feature = "file-store")]
pub mod file;

use crate::checkpoint::{CheckpointHistory, CheckpointId, RevisionId, StoredCheckpoint};
use crate::error::SessionError;
use polaris_system::system::BoxFuture;
use serde::{Deserialize, Serialize};
use std::fmt;
#[cfg(feature = "typegen")]
use ts_rs::TS;

// ─────────────────────────────────────────────────────────────────────────────
// SessionId
// ─────────────────────────────────────────────────────────────────────────────

/// Unique identifier for a session.
///
/// Generated via [`nanoid`] by default, or created from an existing string.
/// Display format is `session_{id}`.
///
/// # Examples
///
/// ```
/// use polaris_sessions::SessionId;
///
/// // Generate a random ID.
/// let id = SessionId::new();
///
/// // Create from a known string (useful for testing or deterministic IDs).
/// let id = SessionId::from_string("my-session-1");
/// assert_eq!(id.as_str(), "my-session-1");
/// assert_eq!(format!("{id}"), "session_my-session-1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct SessionId(String);

impl SessionId {
    /// Creates a new session ID with a random nanoid.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_sessions::SessionId;
    ///
    /// let id = SessionId::new();
    /// assert!(!id.as_str().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(nanoid::nanoid!(8))
    }

    /// Creates a session ID from an existing string.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_sessions::SessionId;
    ///
    /// let id = SessionId::from_string("test-session");
    /// assert_eq!(id.as_str(), "test-session");
    /// ```
    #[must_use]
    pub fn from_string(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Returns the raw ID string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session_{}", self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AgentTypeId
// ─────────────────────────────────────────────────────────────────────────────

/// Identifies an agent type by its stable, user-defined name.
///
/// Wraps the `&'static str` returned by [`Agent::name`](polaris_agent::Agent::name).
///
/// # Examples
///
/// ```
/// use polaris_sessions::AgentTypeId;
///
/// let id = AgentTypeId::from_name("ReActAgent");
/// assert_eq!(id.as_str(), "ReActAgent");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "typegen", derive(TS), ts(export))]
pub struct AgentTypeId(#[cfg_attr(feature = "typegen", ts(type = "string"))] &'static str);

impl AgentTypeId {
    /// Creates an [`AgentTypeId`] from an agent name.
    #[must_use]
    pub fn from_name(name: &'static str) -> Self {
        Self(name)
    }

    /// Returns the agent name string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for AgentTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TurnNumber
// ─────────────────────────────────────────────────────────────────────────────

/// A turn number within a session.
///
/// Alias for `u32` to distinguish turn counts from other numeric quantities
/// such as node counts, durations, or resource indices.
pub type TurnNumber = u32;

// ─────────────────────────────────────────────────────────────────────────────
// ResourceEntry / SessionData
// ─────────────────────────────────────────────────────────────────────────────

/// A single serialized resource entry within session data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEntry {
    /// The plugin that registered this resource.
    pub plugin_id: String,
    /// Stable storage key for the resource type.
    pub storage_key: String,
    /// Schema version of the serialized data.
    pub version: String,
    /// The serialized resource value.
    pub data: serde_json::Value,
}

/// Serialized snapshot of a session's state.
///
/// # Compatibility
///
/// Every checkpoint field below is `#[serde(default)]`, so data written before
/// checkpoints were persisted deserializes with no checkpoints — which is
/// exactly the state a resume produced before. There is no migration step, no
/// version field and no conversion tool. Each is also omitted from the
/// serialized form when empty, so a session with no retained checkpoints
/// writes the same JSON it always did.
///
/// **Breaking change:** this struct is `#[non_exhaustive]`. Out-of-tree code
/// that built one with a struct literal must switch to [`SessionData::new`],
/// then assign the public fields it needs to set. The break is taken now,
/// deliberately, so that later additive fields need no further break; no
/// in-tree or in-graph consumer constructs one.
/// The allocator's raw counter is also private; use
/// [`next_checkpoint_id`](Self::next_checkpoint_id) when inspecting loaded
/// data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionData {
    /// The type name of the agent that owns this session.
    pub agent_type: String,
    /// The turn number at the time of serialization.
    pub turn_number: TurnNumber,
    /// ISO 8601 timestamp of when the session was originally created.
    pub created_at: String,
    /// Serialized resources from the session context.
    pub resources: Vec<ResourceEntry>,

    /// Retained checkpoints, oldest first.
    ///
    /// Bounded per session by
    /// [`CheckpointRetention`](crate::checkpoint::CheckpointRetention): every
    /// save rewrites the whole retained set, so the bound is what keeps that
    /// cost proportional to a budget rather than to the session's age.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<StoredCheckpoint>,

    /// Next checkpoint id to allocate, so checkpoint identity stays unique
    /// across restarts.
    ///
    /// `0` means "written by a version that did not retain checkpoint
    /// identity"; loading normalizes it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) next_checkpoint: u64,

    /// The session's history revision, which increments on every accepted
    /// restore.
    #[serde(default, skip_serializing_if = "RevisionId::is_initial")]
    pub revision: RevisionId,

    /// Checkpoint bookkeeping that outlives the retained payloads: the
    /// session's position, its removal tombstones and their compaction floor.
    ///
    /// Persisted so that a point removed under retention still reports
    /// [`CheckpointStatus::Evicted`](crate::checkpoint::CheckpointStatus::Evicted)
    /// after a restart rather than degrading to
    /// [`Unknown`](crate::checkpoint::CheckpointStatus::Unknown).
    #[serde(default, skip_serializing_if = "CheckpointHistory::is_empty")]
    pub checkpoint_history: CheckpointHistory,
}

impl SessionData {
    /// Creates session data holding no checkpoints.
    ///
    /// The constructor exists because [`SessionData`] is `#[non_exhaustive]`:
    /// it is the supported way to build one outside this crate, and it keeps
    /// later additive fields from breaking callers again.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_sessions::SessionData;
    ///
    /// let data = SessionData::new("ReActAgent", 0, "2026-01-01T00:00:00Z", Vec::new());
    /// assert!(data.resources.is_empty());
    /// assert!(data.checkpoints.is_empty());
    /// ```
    #[must_use]
    pub fn new(
        agent_type: impl Into<String>,
        turn_number: TurnNumber,
        created_at: impl Into<String>,
        resources: Vec<ResourceEntry>,
    ) -> Self {
        Self {
            agent_type: agent_type.into(),
            turn_number,
            created_at: created_at.into(),
            resources,
            checkpoints: Vec::new(),
            next_checkpoint: 0,
            revision: RevisionId::default(),
            checkpoint_history: CheckpointHistory::default(),
        }
    }

    /// Returns the next checkpoint identity that will be allocated, if this
    /// data was written by a checkpoint-aware version.
    ///
    /// The raw counter stays private so checkpoint identity cannot be treated
    /// as an ordinary number by store consumers.
    #[must_use]
    pub fn next_checkpoint_id(&self) -> Option<CheckpointId> {
        (self.next_checkpoint != 0).then(|| CheckpointId::from_raw(self.next_checkpoint))
    }
}

/// Whether a counter has never been advanced, so it can be omitted from the
/// serialized form.
fn is_zero(value: &u64) -> bool {
    *value == 0
}

// ─────────────────────────────────────────────────────────────────────────────
// SessionStore trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for durable session storage backends.
///
/// Implementations must be `Send + Sync + 'static` so they can be shared
/// across threads behind an `Arc`.
///
/// # Examples
///
/// Using the built-in [`InMemoryStore`](memory::InMemoryStore):
///
/// ```
/// use std::sync::Arc;
/// use polaris_sessions::store::memory::InMemoryStore;
/// use polaris_sessions::SessionStore;
///
/// let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
/// ```
pub trait SessionStore: Send + Sync + 'static {
    /// Persists session data under the given ID.
    fn save(&self, id: &SessionId, data: &SessionData) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Loads session data by ID. Returns `Ok(None)` if the session does not exist.
    fn load(&self, id: &SessionId) -> BoxFuture<'_, Result<Option<SessionData>, SessionError>>;

    /// Deletes session data by ID.
    fn delete(&self, id: &SessionId) -> BoxFuture<'_, Result<(), SessionError>>;

    /// Lists all stored session IDs.
    fn list(&self) -> BoxFuture<'_, Result<Vec<SessionId>, SessionError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_checkpoint_state_is_absent_from_serialized_session_data() {
        let data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", Vec::new());
        let value = serde_json::to_value(data).unwrap();
        let object = value.as_object().unwrap();

        for key in [
            "checkpoints",
            "next_checkpoint",
            "revision",
            "checkpoint_history",
        ] {
            assert!(
                !object.contains_key(key),
                "unexpected checkpoint key: {key}"
            );
        }
    }

    #[test]
    fn checkpoint_allocator_is_exposed_as_an_opaque_identity() {
        let mut data = SessionData::new("TestAgent", 0, "2026-01-01T00:00:00Z", Vec::new());
        assert_eq!(data.next_checkpoint_id(), None);
        data.next_checkpoint = 7;
        assert_eq!(data.next_checkpoint_id().map(|id| id.as_u64()), Some(7));
    }
}
