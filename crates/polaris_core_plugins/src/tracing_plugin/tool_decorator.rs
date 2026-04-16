//! Tracing decorator for [`Tool`].
//!
//! [`TracingTool`] decorates any tool with OpenTelemetry-compatible
//! `execute_tool` spans following the `GenAI` semantic conventions.

use polaris_models::llm::ToolDefinition;
use polaris_tools::ToolError;
use polaris_tools::context::ToolContext;
use polaris_tools::tool::Tool;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::Instrument;

/// Decorates a [`Tool`] with tracing instrumentation.
pub(crate) struct TracingTool {
    inner: Arc<dyn Tool>,
    capture_genai_content: bool,
}

impl TracingTool {
    /// Creates a new tracing decorator.
    pub(crate) fn new(inner: Arc<dyn Tool>, capture_genai_content: bool) -> Self {
        Self {
            inner,
            capture_genai_content,
        }
    }
}

impl Tool for TracingTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn execute<'ctx>(
        &'ctx self,
        args: serde_json::Value,
        ctx: &'ctx ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'ctx>> {
        let def = self.inner.definition();
        let span_name = format!("execute_tool {}", def.name);
        let span = tracing::info_span!(
            "execute_tool",
            otel.name = %span_name,
            gen_ai.operation.name = "execute_tool",
            gen_ai.tool.name = %def.name,
            gen_ai.tool.description = %def.description,
            gen_ai.tool.type = "function",
            gen_ai.tool.call.arguments = tracing::field::Empty,
            gen_ai.tool.call.result = tracing::field::Empty,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        );

        let capture_genai_content = self.capture_genai_content;
        let inner = Arc::clone(&self.inner);

        Box::pin(
            async move {
                if capture_genai_content {
                    let current = tracing::Span::current();
                    current.record("gen_ai.tool.call.arguments", args.to_string().as_str());
                }

                let result = inner.execute(args, ctx).await;

                match &result {
                    Ok(value) => {
                        if capture_genai_content {
                            let current = tracing::Span::current();
                            current.record("gen_ai.tool.call.result", value.to_string().as_str());
                        }
                    }
                    Err(tool_err) => {
                        let current = tracing::Span::current();
                        let error_type = tool_err.error_type();
                        if capture_genai_content {
                            let message = tool_err.to_string();
                            current.record("gen_ai.tool.call.result", message.as_str());
                            current.record("otel.status_description", message.as_str());
                        } else {
                            current.record("otel.status_description", error_type);
                        }
                        current.record("error.type", error_type);
                        current.record("otel.status_code", "ERROR");
                    }
                }

                result
            }
            .instrument(span),
        )
    }
}
