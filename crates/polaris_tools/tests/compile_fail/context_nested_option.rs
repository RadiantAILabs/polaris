use polaris_tools::tool;

#[derive(Clone)]
struct SessionId(String);

#[tool]
/// Nested `Option<Option<T>>` context params are rejected — `Option<T>`
/// already expresses "optional context value."
async fn bad_nested_context(
    #[context] session: Option<Option<SessionId>>,
    name: String,
) -> Result<String, polaris_tools::ToolError> {
    let session_str = session
        .flatten()
        .map(|sid| sid.0)
        .unwrap_or_else(|| "anon".to_string());
    Ok(format!("{session_str}: {name}"))
}

fn main() {}
