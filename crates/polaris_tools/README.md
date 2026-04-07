# polaris_tools

Tool framework for Polaris agents.

## Overview

Provides the infrastructure for defining, registering, and executing tools that LLM agents can call. Tools are async functions with automatic JSON schema generation and parameter injection.

- **`#[tool]`** - Attribute macro for standalone tool functions
- **`#[toolset]`** - Attribute macro for grouped tools on impl blocks
- **`Tool`** - Trait for executable tools with JSON schema
- **`ToolRegistry`** - Stores and dispatches tools
- **`ToolsPlugin`** - Manages registry lifecycle

## Quick Start

```rust
use polaris_tools::{tool, ToolsPlugin, ToolRegistry, ToolError};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

#[tool]
/// Search for documents matching a query.
async fn search(
    /// The search query.
    query: String,
    /// Max results to return.
    #[default(10)]
    limit: usize,
) -> Result<String, ToolError> {
    Ok(format!("Found results for: {query}"))
}

// Register in a plugin's build() method:
let mut registry = server.get_resource_mut::<ToolRegistry>().unwrap();
registry.register(search());
```

### Toolsets

Group related tools on an impl block:

```rust
use polaris_tools::{toolset, tool, ToolError};

struct FileTools;

#[toolset]
impl FileTools {
    #[tool]
    /// List files in a directory.
    async fn list_files(&self, path: String) -> Result<String, ToolError> {
        Ok("files".to_string())
    }

    #[tool]
    /// Read a file's contents.
    async fn read_file(&self, path: String) -> Result<String, ToolError> {
        Ok("contents".to_string())
    }
}
```

## License

Apache-2.0
