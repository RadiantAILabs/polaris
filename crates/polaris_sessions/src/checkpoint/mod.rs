//! Durable, identity-stable, bounded session checkpoints.
//!
//! A checkpoint is one session's serialized resource state captured at a turn
//! boundary. This module owns the *identity* of such a point
//! ([`CheckpointId`]), everything known about it except its payload
//! ([`CheckpointMeta`]), the handle that captures and restores them
//! ([`SessionCheckpoints`]), and the policy that bounds how many a session
//! retains ([`CheckpointRetention`]).
//!
//! # Why this exists beside `checkpoint` / `rollback`
//!
//! [`SessionsAPI::checkpoint`](crate::SessionsAPI::checkpoint),
//! [`list_checkpoints`](crate::SessionsAPI::list_checkpoints) and
//! [`rollback`](crate::SessionsAPI::rollback) identify a snapshot by its
//! [`TurnNumber`]. A turn number is a display label, not an
//! identity: two snapshots can carry the same one, selection then resolves to
//! whichever was captured first, and rolling back discards every snapshot with
//! a higher label — so a branch cannot be expressed at all. Those three
//! methods keep their exact behaviour; this module is a new surface beside
//! them, not a reinterpretation of them.
//!
//! What it adds:
//!
//! - **identity** — [`CheckpointId`] is opaque, monotonic per session, never
//!   reused, and never derived from a turn number. [`CheckpointMeta`] stores
//!   [`resume_turn`](CheckpointMeta::resume_turn) — the turn the session runs
//!   *next* if restored here — separately from the display label, so automatic
//!   and manual captures resume identically;
//! - **branches** — [`parent`](CheckpointMeta::parent) and
//!   [`revision`](CheckpointMeta::revision) make "is this point on current
//!   history" a field read rather than a numeric turn comparison, and
//!   [`SessionCheckpoints::restore`] re-parents instead of discarding;
//! - **durability** — the retained set rides
//!   [`SessionData`](crate::SessionData) through the configured
//!   [`SessionStore`](crate::SessionStore), so
//!   [`save_session`](crate::SessionsAPI::save_session) and
//!   [`resume_session`](crate::SessionsAPI::resume_session) carry checkpoints
//!   across a restart with no consumer code change;
//! - **bounds** — [`CheckpointRetention`] bounds one session's retained count
//!   and serialized bytes, evicting or refusing explicitly rather than growing
//!   without limit.
//!
//! # Example
//!
//! ```no_run
//! # use polaris_sessions::{SessionsAPI, SessionId};
//! # use polaris_sessions::checkpoint::CheckpointOrigin;
//! # async fn example(sessions: &SessionsAPI, id: &SessionId) -> Result<(), Box<dyn std::error::Error>> {
//! let checkpoints = sessions.checkpoints();
//!
//! // Capture the current state, then keep it alive across a user decision.
//! let point = checkpoints.capture(id, CheckpointOrigin::Manual).await?;
//! let pin = checkpoints.pin(id, point.id)?;
//!
//! // Later: rewind to it. The state being left is captured first.
//! let outcome = checkpoints.restore(id, point.id).await?;
//! assert_eq!(outcome.resume_turn, point.resume_turn);
//! drop(pin);
//! # Ok(())
//! # }
//! ```

mod handle;
mod retention;
mod store;

pub use handle::{CheckpointPin, SessionCheckpoints};
pub use retention::{CheckpointRetention, CheckpointUsage};
pub use store::{CheckpointHistory, CheckpointTombstone, StoredCheckpoint};

pub(crate) use store::CheckpointSet;

use crate::store::TurnNumber;
use serde::{Deserialize, Serialize};
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// Identity
// ─────────────────────────────────────────────────────────────────────────────

/// Opaque identity of one checkpoint within one session.
///
/// Monotonic per session, never reused, and never derived from a turn number.
/// The counter is persisted with the session, so an id stays unique across
/// restarts.
///
/// There is deliberately no conversion to or from
/// [`TurnNumber`] in either direction: identifying a
/// snapshot by its display label is the defect this type exists to remove, and
/// the conversion is left inexpressible rather than merely discouraged.
///
/// # Examples
///
/// ```
/// use polaris_sessions::checkpoint::CheckpointId;
///
/// # fn example(first: CheckpointId, second: CheckpointId) {
/// // Ids order by allocation, so the newer point compares greater.
/// assert!(second > first);
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckpointId(u64);

impl CheckpointId {
    /// Returns the raw counter value.
    ///
    /// For logging, wire encoding and test assertions only. Callers must not
    /// derive a turn number from it or construct one by arithmetic.
    #[must_use]
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    /// Creates an id from a raw counter value.
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "checkpoint_{}", self.0)
    }
}

/// Monotonic history-revision counter for one session.
///
/// Incremented on every accepted [`restore`](SessionCheckpoints::restore).
/// A checkpoint captured before a restore keeps the revision it was captured
/// on, so the abandoned chain stays listable and is reported as
/// [`CheckpointStatus::OffHistory`] rather than silently discarded.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct RevisionId(u32);

impl RevisionId {
    /// Returns the raw revision number, for logging and test assertions.
    #[must_use]
    pub fn as_u32(&self) -> u32 {
        self.0
    }

    /// Returns the next revision.
    ///
    /// Revision exhaustion is not expected in practice (it requires more
    /// than four billion accepted restores), but it is not silent: the value
    /// saturates and an error is emitted so operators can diagnose the loss of
    /// monotonicity without wrapping an old revision into a new identity.
    pub(crate) fn next(self) -> Self {
        match self.0.checked_add(1) {
            Some(next) => Self(next),
            None => {
                tracing::error!(
                    revision = self.0,
                    "checkpoint revision counter exhausted; keeping the maximum revision"
                );
                self
            }
        }
    }

    /// Whether this is the initial revision a fresh session starts on.
    pub(crate) const fn is_initial(&self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "revision_{}", self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Boundary record
// ─────────────────────────────────────────────────────────────────────────────

/// How a checkpoint came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CheckpointOrigin {
    /// Captured automatically: at session creation, at resume, and after each
    /// completed or interrupted turn while auto-checkpoint is enabled.
    Auto,
    /// Captured by an explicit [`SessionCheckpoints::capture`] or
    /// [`SessionsAPI::checkpoint`](crate::SessionsAPI::checkpoint) call.
    Manual,
    /// Captured immediately before a restore, so the state being left is
    /// always recoverable.
    PreRestore,
}

impl fmt::Display for CheckpointOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::PreRestore => "pre-restore",
        };
        f.write_str(name)
    }
}

/// Everything known about one checkpoint except its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckpointMeta {
    /// Stable identity, unique within the session for the session's lifetime.
    pub id: CheckpointId,

    /// The turn the session will execute **next** if restored to this point.
    ///
    /// This is the field that removes the turn-label off-by-one: it stores
    /// resume semantics rather than a display label, so it is correct for
    /// automatic and manual captures alike. An automatic capture taken after
    /// turn *N* completes carries `resume_turn = N + 1` and `turn = N`; a
    /// manual capture at the same moment carries `resume_turn = N + 1` and
    /// `turn = N + 1`. Both describe the same state and both resume correctly.
    pub resume_turn: TurnNumber,

    /// The display label this point carries in the
    /// [`list_checkpoints`](crate::SessionsAPI::list_checkpoints) view.
    ///
    /// Presentation only. Never a selector: two points may share a label.
    pub turn: TurnNumber,

    /// The nearest **retained** ancestor: the checkpoint that was current when
    /// this one was captured, or `None` for the first point in the session.
    /// This is the branch parentage.
    ///
    /// Eviction re-links children onto the removed point's own parent, so the
    /// retained set stays a tree and this never names a checkpoint that is
    /// gone. A reader that cached the value across an eviction may therefore
    /// see it move up the chain.
    pub parent: Option<CheckpointId>,

    /// The history revision this point was captured on.
    pub revision: RevisionId,

    /// How this point came to exist.
    pub origin: CheckpointOrigin,

    /// ISO 8601 UTC capture time.
    pub taken_at: String,

    /// Serialized size of this checkpoint's payload, in bytes.
    ///
    /// Measured exactly, on the same encoding
    /// [`SessionStore::save`](crate::SessionStore::save) writes, so a byte
    /// budget can never be under-reported.
    pub bytes: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Restore outcomes
// ─────────────────────────────────────────────────────────────────────────────

/// Identifies one serialized resource inside a checkpoint payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResourceRef {
    /// The plugin that registered the resource's serializer.
    pub plugin_id: String,
    /// Stable storage key for the resource type.
    pub storage_key: String,
    /// Schema version the entry was written with.
    pub version: String,
}

impl fmt::Display for ResourceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}::{}@{}",
            self.plugin_id, self.storage_key, self.version
        )
    }
}

/// What a completed restore actually did.
///
/// A restore is not a transaction over resources: only entries whose
/// `(plugin_id, storage_key, schema_version)` triple matches a registered
/// serializer can be loaded. Reporting that plainly is the point of this type
/// — the alternative, silently restoring a subset and returning success, is
/// indistinguishable from a complete restore at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RestoreOutcome {
    /// The point restored to.
    pub target: CheckpointId,

    /// The [`CheckpointOrigin::PreRestore`] checkpoint capturing the state
    /// that was left, captured before the context is touched. Recovery from
    /// an earlier failed restore reuses that failure's coherent original.
    /// Held automatically while a restore remains failed; after success it is
    /// eligible for retention eviction. Use [`SessionCheckpoints::pin`] to
    /// keep it available across subsequent captures.
    pub original: CheckpointId,

    /// The revision the session is on after the restore.
    pub revision: RevisionId,

    /// The turn the session will execute next. Equals the target's
    /// [`resume_turn`](CheckpointMeta::resume_turn).
    pub resume_turn: TurnNumber,

    /// Resources loaded out of the checkpoint into the session context.
    pub restored: Vec<ResourceRef>,

    /// Resources present in the checkpoint that matched **no** registered
    /// serializer and were therefore skipped. Empty on a complete restore;
    /// [`SessionCheckpoints::restore_exact`] refuses rather than reporting a
    /// non-empty list here.
    pub unmatched: Vec<ResourceRef>,

    /// Resources that were live in the session before the restore and have no
    /// entry in the target checkpoint.
    ///
    /// A restore loads the checkpoint's entries over the live context; it does
    /// not clear it. These resources are therefore *not* rewound — they carry
    /// their pre-restore values forward. The framework reports them rather
    /// than re-running [`Agent::setup`](polaris_agent::Agent::setup), which
    /// would replace live policy with defaults.
    pub carried_over: Vec<ResourceRef>,
}

/// Concrete cause of a restore that failed after it began loading resources.
///
/// The variants distinguish the original resource-load failure from the
/// typed refusal returned by later operations while the session remains
/// incoherent. Callers can pattern-match this type without downcasting an
/// erased error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreFailureCause {
    /// A resource serializer failed while applying a checkpoint.
    #[error("resource load failed: {0}")]
    ResourceLoad(#[source] Box<crate::SessionError>),

    /// A later operation was refused because the earlier partial restore left
    /// the live context in an unknown state.
    #[error("session remains incoherent after an earlier restore failure: {reason}")]
    Incoherent {
        /// Display form of the original failure retained by the coherence
        /// latch.
        reason: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Availability
// ─────────────────────────────────────────────────────────────────────────────

/// Whether a checkpoint is still available, and if not, why not.
///
/// A host that offers "rewind to here" needs to distinguish a point it can
/// still reach from one it cannot, and to say which. Guessing is what this
/// type replaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CheckpointStatus {
    /// Retained, on the session's current history path, and restorable.
    Retained(CheckpointMeta),

    /// Known to have existed and to have been removed — evicted to satisfy
    /// [`CheckpointRetention`], or discarded by a legacy
    /// [`rollback`](crate::SessionsAPI::rollback).
    ///
    /// Distinct from [`Unknown`](Self::Unknown) on purpose: a consumer can
    /// tell the user the point is gone rather than that it never existed.
    /// Survives a save/resume round-trip.
    Evicted {
        /// The removed checkpoint.
        id: CheckpointId,
        /// ISO 8601 UTC removal time, or the floor marker's compaction time
        /// once individual tombstones have been compacted.
        evicted_at: String,
    },

    /// Retained, but not on the session's current history path.
    ///
    /// Restorable, but selecting it is a branch switch rather than a rewind,
    /// so a consumer that only offers linear rewind should not present it.
    OffHistory(CheckpointMeta),

    /// Never existed in this session, or was written by a store that did not
    /// retain checkpoint identity.
    Unknown(CheckpointId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tracing_subscriber::{Layer, layer::SubscriberExt};

    struct ErrorEventSeen(Arc<AtomicBool>);

    impl<S> Layer<S> for ErrorEventSeen
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::ERROR {
                self.0.store(true, Ordering::Relaxed);
            }
        }
    }

    fn meta() -> CheckpointMeta {
        CheckpointMeta {
            id: CheckpointId::from_raw(7),
            resume_turn: 3,
            turn: 2,
            parent: Some(CheckpointId::from_raw(6)),
            revision: RevisionId::default(),
            origin: CheckpointOrigin::Manual,
            taken_at: "2026-01-01T00:00:00Z".to_owned(),
            bytes: 42,
        }
    }

    #[test]
    fn status_and_restore_outcome_round_trip_through_json() {
        let status = CheckpointStatus::OffHistory(meta());
        let encoded = serde_json::to_vec(&status).unwrap();
        let decoded: CheckpointStatus = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, status);

        let outcome = RestoreOutcome {
            target: CheckpointId::from_raw(7),
            original: CheckpointId::from_raw(8),
            revision: RevisionId::default(),
            resume_turn: 3,
            restored: vec![ResourceRef {
                plugin_id: "test".to_owned(),
                storage_key: "Counter".to_owned(),
                version: "1.0.0".to_owned(),
            }],
            unmatched: Vec::new(),
            carried_over: Vec::new(),
        };
        let encoded = serde_json::to_vec(&outcome).unwrap();
        let decoded: RestoreOutcome = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, outcome);
    }

    #[test]
    fn exhausted_revision_saturates_loudly_instead_of_wrapping() {
        let error_seen = Arc::new(AtomicBool::new(false));
        let subscriber = tracing_subscriber::registry().with(ErrorEventSeen(error_seen.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);
        let exhausted = RevisionId(u32::MAX);
        assert_eq!(exhausted.next(), exhausted);
        assert!(
            error_seen.load(Ordering::Relaxed),
            "revision exhaustion must emit an error event"
        );
    }
}
