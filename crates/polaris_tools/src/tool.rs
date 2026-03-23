//! The core [`Tool`] trait for executable tools.

use crate::error::ToolError;
use crate::permission::ToolPermission;
use polaris_models::llm::ToolDefinition;
use std::future::Future;
use std::pin::Pin;

/// A tool that can be invoked by an LLM agent.
///
/// Tools expose a [`ToolDefinition`] (name, description, JSON schema) for the LLM,
/// and an async [`execute`](Tool::execute) method that runs with the tool's
/// captured environment.
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

    /// Executes the tool with JSON arguments.
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>;
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

        fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>>
        {
            Box::pin(async { Ok(serde_json::json!("ok")) })
        }
    }

    #[test]
    fn default_permission_is_allow() {
        let tool = DummyTool;
        assert_eq!(tool.permission(), ToolPermission::Allow);
    }
}
