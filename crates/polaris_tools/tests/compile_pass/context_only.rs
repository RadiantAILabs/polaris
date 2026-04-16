use polaris_tools::{tool, ToolError};

#[derive(Clone)]
struct SessionId(String);

#[tool]
/// Tool with only context params and no LLM params.
async fn context_only(#[context] session: SessionId) -> Result<String, ToolError> {
    Ok(format!("session: {}", session.0))
}

fn main() {}
