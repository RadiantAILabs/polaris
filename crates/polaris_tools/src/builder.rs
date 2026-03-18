//! Single-shot tool-use builder extensions.
//!
//! Bridges [`Tool`] / [`Toolset`] into [`LlmRequestBuilder`] and provides
//! [`LlmReasonExt::reason`] for single-shot LLM calls with tool definitions.
//!
//! # Example
//!
//! ```no_run
//! use polaris_tools::{LlmRequestBuilderExt, LlmReasonExt};
//! use polaris_models::llm::Llm;
//!
//! # async fn example(llm: Llm, search: impl polaris_tools::Tool, calculator: impl polaris_tools::Tool) -> Result<(), Box<dyn std::error::Error>> {
//! let response = llm
//!     .builder()
//!     .with_tool(search)
//!     .with_tool(calculator)
//!     .user("What's the weather?")
//!     .reason()
//!     .await?;
//!
//! if response.has_tool_calls() {
//!     for call in response.tool_calls() { /* dispatch */ }
//! } else {
//!     let text = response.text(); /* reply */
//! }
//! # Ok(())
//! # }

use crate::registry::ToolRegistry;
use crate::tool::Tool;
use crate::toolset::Toolset;
use core::future::Future;
use polaris_models::llm::{GenerationError, LlmRequestBuilder, LlmResponse};

// ─────────────────────
// Error
// ─────────────────────

/// Errors that can occur during [`LlmRequestBuilderExt::reason`].
#[derive(Debug, thiserror::Error)]
pub enum ReasonError {
    /// The underlying generation call failed.
    #[error("generation failed: {0}")]
    Generation(#[from] GenerationError),

    /// No tool definitions were provided before calling `reason()`.
    #[error("no tools provided — add at least one tool definition before calling reason()")]
    NoTools,
}

// ─────────────────────
// Extension: Builder chaining (any state)
// ─────────────────────

/// Extension trait on [`LlmRequestBuilder`] for chaining `impl Tool` / `impl Toolset`.
pub trait LlmRequestBuilderExt<'a, S> {
    /// Adds a single tool's definition to the builder.
    fn with_tool(self, tool: impl Tool) -> LlmRequestBuilder<'a, S>;

    /// Adds all tool definitions from a toolset to the builder.
    fn with_toolset(self, toolset: impl Toolset) -> LlmRequestBuilder<'a, S>;

    /// Adds all tool definitions from a registry to the builder.
    fn with_registry(self, registry: &ToolRegistry) -> LlmRequestBuilder<'a, S>;
}

impl<'a, S> LlmRequestBuilderExt<'a, S> for LlmRequestBuilder<'a, S> {
    fn with_tool(self, tool: impl Tool) -> LlmRequestBuilder<'a, S> {
        self.with_definitions(vec![tool.definition()])
    }

    fn with_toolset(self, toolset: impl Toolset) -> LlmRequestBuilder<'a, S> {
        let defs = toolset.tools().iter().map(|t| t.definition()).collect();
        self.with_definitions(defs)
    }

    fn with_registry(self, registry: &ToolRegistry) -> LlmRequestBuilder<'a, S> {
        self.with_definitions(registry.definitions())
    }
}

// ─────────────────────
// Extension: reason() (Ready state only)
// ─────────────────────

/// Extension trait for the [`reason()`](LlmReasonExt::reason) method,
/// available only when the builder is in the [`Ready`](polaris_models::llm::builder::Ready)
/// state (has at least one message).
pub trait LlmReasonExt {
    /// Sends a single LLM call with tool definitions and returns the response.
    ///
    /// Does **not** execute any tools or loop — it only makes the LLM call
    /// with tools available. Use [`LlmResponse::has_tool_calls`] to check
    /// if the model chose to call tools.
    ///
    /// # Errors
    ///
    /// Returns [`ReasonError::NoTools`] if no tool definitions were added.
    /// Returns [`ReasonError::Generation`] if the underlying LLM call fails.
    fn reason(self) -> impl Future<Output = Result<LlmResponse, ReasonError>> + Send;
}

impl LlmReasonExt for LlmRequestBuilder<'_, polaris_models::llm::Ready> {
    async fn reason(self) -> Result<LlmResponse, ReasonError> {
        if self.tool_count() == 0 {
            return Err(ReasonError::NoTools);
        }
        Ok(self.generate().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_models::ModelRegistry;
    use polaris_models::llm::{
        AssistantBlock, GenerationError, Llm, LlmProvider, LlmRequest, LlmResponse, StopReason,
        ToolCall, ToolFunction, Usage,
    };
    use serde_json::json;
    use std::pin::Pin;
    // ── Helpers ──

    fn make_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_id: None,
            function: ToolFunction {
                name: name.to_string(),
                arguments: json!({}),
            },
            signature: None,
            additional_params: None,
        }
    }

    // ── Mock Tool ──

    struct FakeTool {
        name: &'static str,
    }

    impl Tool for FakeTool {
        fn definition(&self) -> polaris_models::llm::ToolDefinition {
            polaris_models::llm::ToolDefinition {
                name: self.name.to_string(),
                description: format!("Fake {}", self.name),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }

        fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, crate::ToolError>> + Send + '_>>
        {
            Box::pin(async { Ok(json!("ok")) })
        }
    }

    // ── Mock Toolset ──

    struct FakeToolset;

    impl Toolset for FakeToolset {
        fn tools(self) -> Vec<Box<dyn Tool>> {
            vec![
                Box::new(FakeTool { name: "alpha" }),
                Box::new(FakeTool { name: "beta" }),
            ]
        }
    }

    // ── Mock Provider ──

    struct ToolCallProvider;

    impl LlmProvider for ToolCallProvider {
        fn name(&self) -> &'static str {
            "mock"
        }

        async fn generate(
            &self,
            _model: &str,
            _request: LlmRequest,
        ) -> Result<LlmResponse, GenerationError> {
            Ok(LlmResponse {
                content: vec![AssistantBlock::ToolCall(make_tool_call("1", "search"))],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
            })
        }
    }

    fn mock_llm() -> Llm {
        let mut registry = ModelRegistry::new();
        registry.register_llm_provider(ToolCallProvider);
        registry.llm("mock/test").unwrap()
    }

    // ── LlmRequestBuilderExt tests ──

    #[test]
    fn chaining_with_tool_accumulates() {
        let llm = mock_llm();
        let builder = llm
            .builder()
            .with_tool(FakeTool { name: "a" })
            .with_tool(FakeTool { name: "b" })
            .with_toolset(FakeToolset);

        // 2 from with_tool + 2 from FakeToolset
        assert_eq!(builder.tool_count(), 4);
    }

    // ── LlmReasonExt tests ──

    #[tokio::test]
    async fn reason_returns_no_tools_error_when_empty() {
        let llm = mock_llm();
        let result: Result<LlmResponse, ReasonError> = llm.builder().user("hello").reason().await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ReasonError::NoTools));
    }

    #[tokio::test]
    async fn reason_returns_response_with_tool_calls() {
        let llm = mock_llm();
        let response = llm
            .builder()
            .with_tool(FakeTool { name: "search" })
            .user("What's the weather?")
            .reason()
            .await
            .unwrap();

        assert!(response.has_tool_calls());
        assert_eq!(response.tool_calls()[0].function.name, "search");
    }
}
