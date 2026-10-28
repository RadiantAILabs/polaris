//! Error types for shell command execution.

use crate::process::ShellProcessId;
use thiserror::Error;

/// Errors that can occur during shell command execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ShellError {
    /// Failed to spawn the shell process.
    #[error("failed to spawn command: {0}")]
    SpawnFailed(String),

    /// I/O error during process execution.
    #[error("I/O error: {0}")]
    IoError(String),

    /// The working directory is invalid or inaccessible.
    #[error("invalid working directory: {0}")]
    InvalidWorkingDir(String),

    /// The command was denied by the permission policy.
    #[error("command denied: {0}")]
    PermissionDenied(String),

    /// The event sink was already claimed by another run.
    #[error("shell event sink was already used by another run")]
    SinkAlreadyUsed,

    /// The command string exceeds the configured maximum length.
    #[error("command too long: {length} bytes exceeds limit of {max} bytes")]
    CommandTooLong {
        /// Actual command length in bytes.
        length: usize,
        /// Configured maximum.
        max: usize,
    },

    /// The terminal result of a managed run was consumed by another waiter.
    ///
    /// Terminal results are consumed exactly once. Foreground runs started by
    /// [`ShellExecutor::execute`](crate::ShellExecutor::execute) and
    /// [`execute_confirmed`](crate::ShellExecutor::execute_confirmed) reserve
    /// their result for the foreground caller, so
    /// [`ShellExecutor::wait_for`](crate::ShellExecutor::wait_for) never takes
    /// it and this variant is not reachable through the reserved path. It
    /// exists so those two methods report the loss as an error rather than
    /// panicking if a future change ever reopens the race.
    #[error("terminal result for shell process {0:?} was already consumed")]
    ResultConsumed(ShellProcessId),
}
