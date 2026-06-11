use polaris_tools::{Tool, ToolError, Toolset, tool, toolset};

#[tool(strict = false)]
/// A standalone tool that opts out of strict mode.
async fn loose(name: String) -> Result<String, ToolError> {
    Ok(name)
}

#[tool]
/// A standalone tool with the default (strict) mode.
async fn tight(name: String) -> Result<String, ToolError> {
    Ok(name)
}

struct FileTools;

#[toolset]
impl FileTools {
    #[tool(strict = false)]
    /// A toolset method that opts out of strict mode.
    async fn loose_method(&self, path: String) -> Result<String, ToolError> {
        Ok(path)
    }

    #[tool]
    /// A toolset method with the default (strict) mode.
    async fn tight_method(&self, path: String) -> Result<String, ToolError> {
        Ok(path)
    }
}

fn main() {
    assert!(!loose().definition().strict, "strict = false is honored");
    assert!(tight().definition().strict, "strict defaults to true");

    let tools = FileTools.tools();
    let strict_of = |name: &str| {
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .unwrap()
            .definition()
            .strict
    };
    assert!(!strict_of("loose_method"), "per-method strict = false honored");
    assert!(strict_of("tight_method"), "per-method strict defaults true");
}
