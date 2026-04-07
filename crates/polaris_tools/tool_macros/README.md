# tool_macros

Procedural macros for the Polaris tool framework.

## Overview

Provides the `#[tool]` and `#[toolset]` attribute macros used by `polaris_tools`. This is a proc-macro crate — it is re-exported by `polaris_tools` and should not be depended on directly.

### `#[tool]`

Transforms an async function into a `Tool` implementation with automatic JSON schema generation and parameter extraction.

```rust
use polaris_tools::{tool, ToolError};

#[tool]
/// Search for documents.
async fn search(
    /// The search query.
    query: String,
    /// Max results.
    #[default(10)]
    limit: usize,
) -> Result<String, ToolError> {
    Ok(format!("Results for: {query}"))
}
```

### `#[toolset]`

Transforms an impl block into a `Toolset` that provides all `#[tool]` methods as individual `Tool` instances for bulk registration.

```rust
use polaris_tools::{toolset, tool, ToolError};

struct FileTools;

#[toolset]
impl FileTools {
    #[tool]
    /// List files.
    async fn list_files(&self, path: String) -> Result<String, ToolError> {
        Ok("files".to_string())
    }
}
```

## License

Apache-2.0
