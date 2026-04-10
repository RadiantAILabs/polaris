//! Error types for graph execution.

use crate::node::NodeId;
use crate::predicate::PredicateError;
use polaris_system::param::{AccessMode, ErrorContext};
use std::any::TypeId;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Errors that can occur during graph execution.
#[derive(Debug, Clone)]
pub enum ExecutionError {
    /// The graph has no entry point.
    EmptyGraph,
    /// A referenced node was not found in the graph.
    NodeNotFound(NodeId),
    /// No sequential edge found from the given node.
    NoNextNode(NodeId),
    /// A decision or loop node is missing its predicate.
    MissingPredicate(NodeId),
    /// A decision node is missing a branch target.
    MissingBranch {
        /// The node ID of the decision node.
        node: NodeId,
        /// Which branch is missing ("true" or "false").
        branch: &'static str,
    },
    /// A system execution error occurred.
    SystemError(Arc<str>),
    /// A predicate evaluation error occurred.
    PredicateError(PredicateError),
    /// Maximum iterations exceeded in a loop.
    MaxIterationsExceeded {
        /// The loop node that exceeded iterations.
        node: NodeId,
        /// The maximum allowed iterations.
        max: usize,
    },
    /// A loop node has no termination condition (neither predicate nor `max_iterations`).
    NoTerminationCondition(NodeId),
    /// A system execution timed out.
    Timeout {
        /// The node that timed out.
        node: NodeId,
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },
    /// Feature not yet implemented.
    Unimplemented(&'static str),
    /// Maximum recursion depth exceeded in nested control flow.
    RecursionLimitExceeded {
        /// The current depth when the limit was hit.
        depth: usize,
        /// The maximum allowed depth.
        max: usize,
    },
    /// A switch node is missing its discriminator.
    MissingDiscriminator(NodeId),
    /// No matching case found in switch node and no default provided.
    NoMatchingCase {
        /// The switch node ID.
        node: NodeId,
        /// The discriminator value that didn't match any case.
        key: &'static str,
    },
    /// An internal framework invariant was violated.
    InternalError(String),
    /// A middleware layer failed.
    MiddlewareError {
        /// Registered name of the middleware that failed.
        middleware: String,
        /// Description of the failure.
        message: String,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::EmptyGraph => write!(f, "graph has no entry point"),
            ExecutionError::NodeNotFound(id) => write!(f, "node not found: {id}"),
            ExecutionError::NoNextNode(id) => write!(f, "no sequential edge from node: {id}"),
            ExecutionError::MissingPredicate(id) => {
                write!(f, "missing predicate on node: {id}")
            }
            ExecutionError::MissingBranch { node, branch } => {
                write!(f, "missing {branch} branch on decision node: {node}")
            }
            ExecutionError::SystemError(msg) => write!(f, "system error: {msg}"),
            ExecutionError::PredicateError(err) => write!(f, "predicate error: {err}"),
            ExecutionError::MaxIterationsExceeded { node, max } => {
                write!(f, "max iterations ({max}) exceeded on loop node: {node}")
            }
            ExecutionError::NoTerminationCondition(id) => {
                write!(f, "loop node has no termination condition: {id}")
            }
            ExecutionError::Timeout { node, timeout } => {
                write!(f, "system timed out after {:?} on node: {node}", timeout)
            }
            ExecutionError::Unimplemented(feature) => {
                write!(f, "feature not implemented: {feature}")
            }
            ExecutionError::RecursionLimitExceeded { depth, max } => {
                write!(
                    f,
                    "recursion limit exceeded: depth {depth} exceeds max {max}"
                )
            }
            ExecutionError::MissingDiscriminator(id) => {
                write!(f, "missing discriminator on switch node: {id}")
            }
            ExecutionError::NoMatchingCase { node, key } => {
                write!(f, "no matching case for key '{key}' on switch node: {node}")
            }
            ExecutionError::InternalError(msg) => write!(f, "internal error: {msg}"),
            ExecutionError::MiddlewareError {
                middleware,
                message,
            } => {
                write!(f, "middleware '{middleware}' failed: {message}")
            }
        }
    }
}

impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExecutionError::PredicateError(err) => Some(err),
            _ => None,
        }
    }
}

/// Errors that can occur during resource validation.
///
/// These errors are detected before graph execution starts, allowing
/// early detection of missing resources that would cause runtime failures.
#[derive(Debug, Clone)]
pub enum ResourceValidationError {
    /// A required resource is missing from the context.
    MissingResource {
        /// The node ID of the system requiring the resource.
        node: NodeId,
        /// The name of the system.
        system_name: &'static str,
        /// The type name of the missing resource.
        resource_type: &'static str,
        /// The type ID of the missing resource.
        type_id: TypeId,
        /// The access mode (read or write).
        access_mode: AccessMode,
    },
    /// A required output from a previous system is missing.
    MissingOutput {
        /// The node ID of the system requiring the output.
        node: NodeId,
        /// The name of the system.
        system_name: &'static str,
        /// The type name of the missing output.
        output_type: &'static str,
        /// The type ID of the missing output.
        type_id: TypeId,
    },
}

impl fmt::Display for ResourceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceValidationError::MissingResource {
                node,
                system_name,
                resource_type,
                access_mode,
                ..
            } => {
                let mode_str = match access_mode {
                    AccessMode::Read => "read",
                    AccessMode::Write => "write",
                };
                write!(
                    f,
                    "system '{system_name}' ({node}) requires {mode_str} access to missing resource: {resource_type}"
                )
            }
            ResourceValidationError::MissingOutput {
                node,
                system_name,
                output_type,
                ..
            } => {
                write!(
                    f,
                    "system '{system_name}' ({node}) requires missing output: {output_type}"
                )
            }
        }
    }
}

impl std::error::Error for ResourceValidationError {}

/// Classification of the error that caused a system failure.
///
/// Used in [`CaughtError`] to distinguish error sources without parsing
/// message strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// System returned `Err(SystemError::ExecutionError(...))`.
    Execution,
    /// System parameter resolution failed (`Err(SystemError::ParamError(...))`).
    ParamResolution,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Execution => write!(f, "execution"),
            ErrorKind::ParamResolution => write!(f, "param_resolution"),
        }
    }
}

/// Error context injected by the executor when routing to an error handler.
///
/// When a system fails and an error edge exists, the executor stores this
/// in the outputs before routing to the handler node. Error handler systems
/// read it via [`ErrOut<CaughtError>`](polaris_system::param::ErrOut).
///
/// # Fields
///
/// - `message` — The error message from the failed system
/// - `system_name` — The name of the system that failed
/// - `node_id` — The graph node ID of the failed system
/// - `duration` — How long the system ran before failing
/// - `kind` — Classification of the error source
///
/// # Example
///
/// ```
/// use polaris_graph::CaughtError;
/// use polaris_system::param::ErrOut;
/// use polaris_system::system;
///
/// # #[derive(Default)]
/// # struct RecoveryState;
///
/// #[system]
/// async fn handle_error(error: ErrOut<CaughtError>) -> RecoveryState {
///     tracing::error!("[{}] {}: {}", error.node_id, error.system_name, error.message);
///     RecoveryState::default()
/// }
/// ```
#[derive(Debug, Clone)]
pub struct CaughtError {
    /// The error message from the failed system.
    pub message: Arc<str>,
    /// The name of the system that failed.
    pub system_name: &'static str,
    /// The node ID of the failed system.
    pub node_id: NodeId,
    /// How long the system ran before failing.
    pub duration: Duration,
    /// Classification of the error source.
    pub kind: ErrorKind,
}

impl fmt::Display for CaughtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "system '{}' ({}) failed after {:?} [{}]: {}",
            self.system_name, self.node_id, self.duration, self.kind, self.message
        )
    }
}

impl std::error::Error for CaughtError {}

impl ErrorContext for CaughtError {}

/// Internal result of executing a system with optional retry and timeout.
pub(crate) enum SystemOutcome {
    /// System completed successfully.
    Ok(Box<dyn core::any::Any + Send + Sync>),
    /// System failed with an error after all retry attempts.
    Err(polaris_system::system::SystemError),
    /// System timed out after all retry attempts.
    Timeout,
}
