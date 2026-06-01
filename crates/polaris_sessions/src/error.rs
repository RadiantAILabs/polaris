//! Error types for session operations.

use crate::store::{SessionId, TurnNumber};
use polaris_agent::SetupError;
use polaris_graph::ValidationResult;

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

    /// No checkpoint exists for the given turn number.
    #[error("turn not found: {0}")]
    TurnNotFound(TurnNumber),

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
