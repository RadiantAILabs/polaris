//! Error types for session operations.

use crate::api::{ContractName, ContractNameError};
use crate::checkpoint::{CheckpointId, CheckpointRetention, ResourceRef, RestoreFailureCause};
use crate::store::{SessionId, TurnNumber};
use polaris_agent::SetupError;
use polaris_graph::{RunId, ValidationResult};
use std::time::Duration;

/// Errors that can occur during session operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// A persistence operation (serialization/deserialization) failed.
    #[error("persistence error: {0}")]
    Persistence(#[from] polaris_core_plugins::persistence::PersistenceError),

    /// Graph execution failed.
    #[error("execution error: {0}")]
    Execution(#[from] polaris_graph::ExecutionError),

    /// The underlying store encountered an error.
    #[error("store error: {0}")]
    Store(Box<dyn std::error::Error + Send + Sync>),

    /// No session exists with the given ID.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// The session is already executing a turn.
    #[error("session busy: {0}")]
    SessionBusy(SessionId),

    /// The turn was interrupted through
    /// [`SessionsAPI::interrupt_turn`](crate::SessionsAPI::interrupt_turn).
    ///
    /// Distinct from [`Execution`](Self::Execution) on purpose: an interrupt
    /// is a user exercising control, not a system failing, and a driver needs
    /// to tell the two apart. The turn was recorded, the session is idle,
    /// `turn_number` advanced, and the next turn may proceed.
    ///
    /// An automatic checkpoint of the interrupted state is *attempted*, and
    /// taken unless auto-checkpoint is disabled or
    /// [`CheckpointRetention`] refuses
    /// it for capacity — the refusal is warned rather than fatal, so the
    /// interrupt contract holds either way. Use
    /// [`SessionCheckpoints::current`](crate::checkpoint::SessionCheckpoints::current)
    /// to see whether a point was actually retained instead of assuming one
    /// was.
    ///
    /// `SessionError::Execution(ExecutionError::Interrupted { .. })` is never
    /// constructed — this is the one representation of an interrupted turn.
    #[error("turn {turn} of session {session} was interrupted after {nodes_executed} nodes")]
    Interrupted {
        /// The session whose turn was interrupted.
        session: SessionId,
        /// The turn number that was interrupted. It will not be reused.
        turn: TurnNumber,
        /// The interrupted run, matching every `GraphEvent` it emitted.
        run_id: RunId,
        /// Nodes that started before the stop.
        nodes_executed: usize,
        /// Time from run start to the stop.
        elapsed: Duration,
    },

    /// The session is read-only and rejects mutation.
    ///
    /// Returned by methods that would change session state (e.g.
    /// `process_turn`, `rollback`, `setup_session`, `resume_session`,
    /// `with_context`) when invoked against a session preserved by
    /// [`SessionsAPI::run_oneshot_preserved`](crate::SessionsAPI::run_oneshot_preserved).
    #[error("session is read-only: {0}")]
    ReadOnly(SessionId),

    /// A session with the given ID already exists.
    #[error("session already exists: {0}")]
    SessionAlreadyExists(SessionId),

    /// No agent has been registered with the given type name.
    #[error("agent not found: {0}")]
    AgentNotFound(String),

    /// A capability contract name was empty or not normalized.
    #[error(transparent)]
    InvalidContractName(#[from] ContractNameError),

    /// No capability contract has been registered with the given name.
    #[error("contract not found: {0}")]
    ContractNotFound(ContractName),

    /// A capability contract name was re-registered with a different
    /// signature.
    ///
    /// Contracts are cross-plugin coordination points, so
    /// [`SessionsAPI::register_contract`](crate::SessionsAPI::register_contract)
    /// refuses to silently replace one; re-registering an identical
    /// signature is a no-op.
    #[error("contract '{name}' is already registered with a different signature")]
    ContractConflict {
        /// Name of the contract that was re-registered.
        name: ContractName,
    },

    /// No checkpoint exists for the given turn number.
    ///
    /// The legacy [`rollback`](crate::SessionsAPI::rollback) error. The
    /// checkpoint surface uses [`CheckpointNotFound`](Self::CheckpointNotFound)
    /// instead, which identifies a snapshot rather than a label.
    #[error("turn not found: {0}")]
    TurnNotFound(TurnNumber),

    /// No retained checkpoint has the given id.
    ///
    /// Never conflated with "evicted": ask
    /// [`SessionCheckpoints::status`](crate::checkpoint::SessionCheckpoints::status)
    /// which of the two it is, so a consumer can tell a user *why* a rewind
    /// point is unavailable.
    #[error("checkpoint {checkpoint} not found in session {session}")]
    CheckpointNotFound {
        /// The session that was searched.
        session: SessionId,
        /// The checkpoint that is not retained.
        checkpoint: CheckpointId,
    },

    /// A [`restore_exact`](crate::checkpoint::SessionCheckpoints::restore_exact)
    /// could not restore every serialized resource in the checkpoint.
    ///
    /// **Nothing was written to the context.** The check runs against the
    /// registered serializer list before any resource is loaded, so a refused
    /// exact restore never half-applies.
    #[error("restore of checkpoint {target} in session {session} would be incomplete: {} resources have no registered serializer", unmatched.len())]
    RestoreIncomplete {
        /// The session that was to be restored.
        session: SessionId,
        /// The checkpoint that was to be restored.
        target: CheckpointId,
        /// The entries no registered serializer matched.
        unmatched: Vec<ResourceRef>,
    },

    /// A resource failed to load partway through a restore.
    ///
    /// The context is in an **unknown** state: some resources are at the
    /// target's values and some are not. The session refuses further turns
    /// until a restore succeeds or the caller resumes from the store, and
    /// `original` is the recovery route — it is the pre-restore checkpoint,
    /// held against eviction for exactly this reason.
    #[error("restore of checkpoint {target} in session {session} failed: {source}")]
    RestoreFailed {
        /// The session whose context is now unknown.
        session: SessionId,
        /// The checkpoint that was being restored.
        target: CheckpointId,
        /// The pre-restore checkpoint capturing the state that was left.
        original: CheckpointId,
        /// The concrete load failure or later coherence refusal.
        source: RestoreFailureCause,
    },

    /// A checkpoint could not be retained within the configured bounds.
    ///
    /// Raised instead of silently dropping the oldest point a caller might
    /// still need. An explicit
    /// [`capture`](crate::checkpoint::SessionCheckpoints::capture) — and
    /// therefore a restore's pre-restore snapshot — fails loudly; an automatic
    /// checkpoint warns instead, so a bound never fails a user's turn.
    #[error("checkpoint capacity exhausted for session {session}: at least {needed_bytes} bytes needed, {pinned} retained checkpoints pinned, limit {:?} checkpoints / {:?} bytes", retention.max_count, retention.max_bytes)]
    CheckpointCapacity {
        /// The session whose budget is exhausted.
        session: SessionId,
        /// Serialized bytes the refused checkpoint needed, or the smallest
        /// known size when measurement stopped at the configured budget.
        needed_bytes: usize,
        /// The policy that refused it.
        retention: CheckpointRetention,
        /// Retained checkpoints that could not be evicted.
        pinned: usize,
    },

    /// Agent setup failed during session creation or resume.
    #[error("agent setup failed for '{agent_name}': {source}")]
    Setup {
        /// Name of the agent whose setup failed.
        agent_name: String,
        /// The underlying setup error.
        source: SetupError,
    },

    /// The agent's graph failed validation.
    #[error("graph validation failed for agent '{agent_name}': {result}")]
    GraphValidation {
        /// Name of the agent whose graph failed validation.
        agent_name: String,
        /// The validation result containing errors and warnings.
        result: ValidationResult,
    },

    /// The graph completed but did not produce the expected output type.
    #[error("output not found: expected {0}")]
    OutputNotFound(&'static str),
}

/// Errors returned by [`SessionsAPI`](crate::SessionsAPI) plugin-wiring
/// methods when the same wiring step is performed twice.
///
/// Returned by [`SessionsAPI::set_graph_apis`](crate::SessionsAPI::set_graph_apis)
/// and [`SessionsAPI::set_context_factory`](crate::SessionsAPI::set_context_factory).
/// `SessionsPlugin` itself invokes the setters once during `ready()`, so this
/// error only surfaces for callers wiring `SessionsAPI` manually outside the
/// plugin.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WiringError {
    /// Graph hooks API was already wired.
    #[error("graph hooks API already wired")]
    HooksAlreadySet,
    /// Graph middleware API was already wired.
    #[error("graph middleware API already wired")]
    MiddlewareAlreadySet,
    /// Context factory was already wired.
    #[error("context factory already wired")]
    ContextFactoryAlreadySet,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_errors_have_stable_single_spaced_messages() {
        let session = SessionId::from_string("test");
        let incomplete = SessionError::RestoreIncomplete {
            session: session.clone(),
            target: CheckpointId::from_raw(7),
            unmatched: vec![ResourceRef {
                plugin_id: "plugin".to_owned(),
                storage_key: "Resource".to_owned(),
                version: "1.0.0".to_owned(),
            }],
        };
        assert_eq!(
            incomplete.to_string(),
            "restore of checkpoint checkpoint_7 in session session_test would be incomplete: 1 resources have no registered serializer"
        );

        let capacity = SessionError::CheckpointCapacity {
            session,
            needed_bytes: 123,
            retention: CheckpointRetention::default(),
            pinned: 2,
        };
        assert_eq!(
            capacity.to_string(),
            "checkpoint capacity exhausted for session session_test: at least 123 bytes needed, 2 retained checkpoints pinned, limit Some(64) checkpoints / Some(16777216) bytes"
        );
    }
}
