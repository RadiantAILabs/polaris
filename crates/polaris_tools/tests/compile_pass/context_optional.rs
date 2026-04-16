use polaris_tools::{tool, ToolError};

#[derive(Clone)]
struct DryRun(bool);

#[tool]
/// Optional context param — should compile and be None when absent.
async fn maybe_write(
    #[context] dry_run: Option<DryRun>,
    /// The payload to write.
    payload: String,
) -> Result<String, ToolError> {
    match dry_run {
        Some(DryRun(true)) => Ok(format!("would write: {payload}")),
        _ => Ok(format!("wrote: {payload}")),
    }
}

fn main() {}
