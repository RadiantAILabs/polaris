//! Error types for tool execution.

use thiserror::Error;

/// Errors that can occur during tool execution.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// A registry configuration or setup operation failed.
    ///
    /// Used for build-phase / administrative errors such as invalid
    /// permission overrides or conflicting registrations.
    #[error("Registry error: {0}")]
    RegistryError(String),

    /// JSON serialization/deserialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    /// A tool was not found during runtime execution.
    ///
    /// Used when a caller (typically an LLM agent) requests a tool
    /// name that does not exist in the registry.
    #[error("Unknown tool: {0}")]
    UnknownTool(String),
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

    /// Creates an [`UnknownTool`](Self::UnknownTool).
    pub fn unknown_tool(name: impl Into<String>) -> Self {
        Self::UnknownTool(name.into())
    }

    /// Creates a [`RegistryError`](Self::RegistryError).
    pub fn registry_error(msg: impl Into<String>) -> Self {
        Self::RegistryError(msg.into())
    }

    /// Returns a low-cardinality error type string.
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::ParameterError(_) | Self::SerializationError(_) => "validation_error",
            Self::ExecutionError(_) | Self::ResourceNotFound(_) => "execution_error",
            Self::PermissionDenied(_) => "permission_denied",
            Self::RegistryError(_) => "registry_error",
            Self::UnknownTool(_) => "unknown_tool",
        }
    }
}
