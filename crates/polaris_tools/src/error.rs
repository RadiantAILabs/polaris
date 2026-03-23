//! Error types for tool execution.

use thiserror::Error;

/// Errors that can occur during tool execution.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Error during parameter deserialization or parsing.
    #[error("Parameter error: {0}")]
    ParameterError(String),

    /// Error during tool function execution.
    #[error("Execution error: {0}")]
    ExecutionError(String),

    /// The tool invocation was denied by its permission policy.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// A required resource was not found in the system context.
    #[error("Resource not found: {0}")]
    ResourceNotFound(String),

    /// Error during tool registry operations (e.g., unknown tool name).
    #[error("Registry error: {0}")]
    RegistryError(String),

    /// JSON serialization/deserialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

impl ToolError {
    /// Creates a [`ParameterError`](Self::ParameterError).
    pub fn parameter_error(msg: impl Into<String>) -> Self {
        Self::ParameterError(msg.into())
    }

    /// Creates an [`ExecutionError`](Self::ExecutionError).
    pub fn execution_error(msg: impl Into<String>) -> Self {
        Self::ExecutionError(msg.into())
    }

    /// Creates a [`PermissionDenied`](Self::PermissionDenied).
    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self::PermissionDenied(msg.into())
    }

    /// Creates a [`ResourceNotFound`](Self::ResourceNotFound).
    pub fn resource_not_found(type_name: impl Into<String>) -> Self {
        Self::ResourceNotFound(type_name.into())
    }

    /// Creates a [`RegistryError`](Self::RegistryError).
    pub fn registry_error(msg: impl Into<String>) -> Self {
        Self::RegistryError(msg.into())
    }
}
