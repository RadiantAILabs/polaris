//! The core [`Tool`] trait for executable tools.

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::permission::ToolPermission;
use polaris_models::llm::ToolDefinition;
use std::future::Future;
use std::pin::Pin;

/// A tool that can be invoked by an LLM agent.
///
/// Tools expose a [`ToolDefinition`] (name, description, JSON schema) for the LLM,
/// and an async [`execute`](Tool::execute) method that runs with the tool's
/// captured environment and an optional [`ToolContext`] for per-invocation state.
///
/// # Examples
///
/// ```
/// use polaris_tools::{Tool, ToolContext, ToolError};
/// use polaris_models::llm::ToolDefinition;
/// use serde_json::{json, Value};
/// use std::pin::Pin;
/// use std::future::Future;
///
/// struct EchoTool;
///
/// impl Tool for EchoTool {
///     fn definition(&self) -> ToolDefinition {
///         ToolDefinition {
///             name: "echo".into(),
///             description: "Echoes input back".into(),
///             parameters: json!({"type": "object", "properties": {
///                 "text": {"type": "string"}
///             }}),
///         }
///     }
///
///     fn execute<'ctx>(
///         &'ctx self,
///         args: Value,
///         _ctx: &'ctx ToolContext,
///     ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'ctx>> {
///         Box::pin(async move { Ok(args) })
///     }
/// }
/// ```
pub trait Tool: Send + Sync + 'static {
    /// Returns the LLM-facing tool definition with JSON schema.
    fn definition(&self) -> ToolDefinition;

    /// Returns the default permission level for this tool.
    ///
    /// The default implementation returns [`ToolPermission::Allow`], allowing
    /// unrestricted execution. Override this to require confirmation or deny
    /// execution entirely.
    ///
    /// Runtime systems may further adjust permissions beyond this default
    /// (e.g., per-agent config, external config files).
    fn permission(&self) -> ToolPermission {
        ToolPermission::Allow
    }

    /// Executes the tool with JSON arguments and per-invocation context.
    ///
    /// The [`ToolContext`] carries per-invocation state supplied by the
    /// calling system — anything the tool needs that shouldn't appear in
    /// the LLM-facing argument schema (e.g., a session ID, a working
    /// directory, a dry-run flag, an opaque backend handle). Tools that
    /// don't need per-invocation state can ignore the context parameter.
    ///
    /// For `#[tool]` macro-generated tools, context parameters are declared
    /// with the `#[context]` attribute and extracted automatically.
    ///
    /// # Errors
    ///
    /// Implementations commonly return:
    /// - [`ToolError::ParameterError`] or [`ToolError::SerializationError`]
    ///   if `args` cannot be parsed into the tool's parameter types.
    /// - [`ToolError::ResourceNotFound`] if a required `#[context]` value
    ///   is not present in `ctx`.
    /// - [`ToolError::ExecutionError`] if the tool body fails.
    ///
    /// [`ToolError::ParameterError`]: ToolError::ParameterError
    /// [`ToolError::SerializationError`]: ToolError::SerializationError
    /// [`ToolError::ResourceNotFound`]: ToolError::ResourceNotFound
    /// [`ToolError::ExecutionError`]: ToolError::ExecutionError
    fn execute<'ctx>(
        &'ctx self,
        args: serde_json::Value,
        ctx: &'ctx ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'ctx>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;

    impl Tool for DummyTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "dummy".into(),
                description: "A dummy tool for testing".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn execute<'ctx>(
            &'ctx self,
            _args: serde_json::Value,
            _ctx: &'ctx ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'ctx>>
        {
            Box::pin(async { Ok(serde_json::json!("ok")) })
        }
    }

    #[test]
    fn default_permission_is_allow() {
        let tool = DummyTool;
        assert_eq!(tool.permission(), ToolPermission::Allow);
    }

    #[test]
    fn tool_is_dyn_compatible() {
        let _boxed: Box<dyn Tool> = Box::new(DummyTool);
        let _arced: std::sync::Arc<dyn Tool> = std::sync::Arc::new(DummyTool);
    }
}
