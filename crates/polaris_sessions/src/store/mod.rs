//! Session storage traits and types.
//!
//! This module defines the [`SessionStore`] trait for persisting session data,
//! along with the core identity and data types used across the sessions crate.

pub mod memory;

#[cfg(feature = "file-store")]
pub mod file;

use crate::error::SessionError;
use polaris_system::system::BoxFuture;
use serde::{Deserialize, Serialize};
use std::fmt;

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
/// Wraps the `&'static str` returned by [`Agent::name`].
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
pub struct AgentTypeId(&'static str);

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    /// The type name of the agent that owns this session.
    pub agent_type: String,
    /// The turn number at the time of serialization.
    pub turn_number: TurnNumber,
    /// ISO 8601 timestamp of when the session was originally created.
    pub created_at: String,
    /// Serialized resources from the session context.
    pub resources: Vec<ResourceEntry>,
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
