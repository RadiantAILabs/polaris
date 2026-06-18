use polaris_tools::{ToolError, toolset};

struct FileTools;

#[toolset]
impl FileTools {
    #[tool = false]
    /// `#[tool]` takes options in `#[tool(strict = false)]` form, not name-value.
    async fn bad(&self, path: String) -> Result<String, ToolError> {
        Ok(path)
    }
}

fn main() {}
