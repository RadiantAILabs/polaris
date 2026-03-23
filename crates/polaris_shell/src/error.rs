//! Error types for shell command execution.

use thiserror::Error;

/// Errors that can occur during shell command execution.
#[derive(Debug, Error)]
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

    /// The command string exceeds the configured maximum length.
    #[error("command too long: {length} bytes exceeds limit of {max} bytes")]
    CommandTooLong {
        /// Actual command length in bytes.
        length: usize,
        /// Configured maximum.
        max: usize,
    },
}
