use polaris_tools::{tool, ToolError};

#[derive(Clone)]
struct SessionId(String);

#[tool]
/// Standalone tool with a context param.
async fn with_context(
    #[context] session: SessionId,
    /// The message.
    message: String,
) -> Result<String, ToolError> {
    Ok(format!("[{}] {}", session.0, message))
}

fn main() {}
