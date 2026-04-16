use polaris_tools::{tool, ToolError};

/// A type that does NOT implement Clone.
struct NotClone(String);

#[tool]
/// Context param must be Clone — this should fail.
async fn bad_context(
    #[context] ctx: NotClone,
    name: String,
) -> Result<String, ToolError> {
    Ok(format!("{}: {}", ctx.0, name))
}

fn main() {}
