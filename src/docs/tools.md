Tool framework for LLM-callable functions.

This module provides infrastructure for defining, registering, and executing
tools that LLMs can invoke. Tools are async functions with automatic JSON
schema generation, parameter injection, and a permission model.

# Defining Tools

**Standalone function** with the `#[tool]` macro:

```ignore
# use polaris_ai::polaris_system;
use polaris_ai::tools::{tool, ToolError};

#[tool]
/// Search for documents matching a query.
async fn search(
    /// The search query.
    query: String,
    /// Max results to return.
    #[default(10)]
    limit: usize,
) -> Result<String, ToolError> {
    Ok(format!("Found {limit} results for: {query}"))
}
```

The macro generates a struct implementing [`Tool`](crate::tools::Tool), extracts doc comments as
descriptions, builds a JSON schema from argument types, and wires `#[default]`
values as optional parameters.

**Toolsets** group related tools sharing state:

```ignore
# use polaris_ai::polaris_system;
use polaris_ai::tools::{toolset, tool, ToolError};

struct FileTools {
    root: std::path::PathBuf,
}

#[toolset]
impl FileTools {
    #[tool]
    /// List files in a directory.
    async fn list_files(&self, path: String) -> Result<Vec<String>, ToolError> {
        Ok(vec![])
    }

    #[tool]
    /// Read a file's contents.
    async fn read_file(&self, path: String) -> Result<String, ToolError> {
        Ok(String::new())
    }
}
```

# Registration

Register tools in a plugin's `build()` method via [`ToolRegistry`](crate::tools::ToolRegistry):

```ignore
# use polaris_ai::polaris_system;
# use polaris_ai::tools::{tool, ToolError, ToolsPlugin, ToolRegistry};
# use polaris_ai::system::plugin::{Plugin, PluginId, Version};
# use polaris_ai::system::server::Server;
# #[tool]
# /// Search.
# async fn search(query: String) -> Result<String, ToolError> { Ok(query) }
struct SearchPlugin;

impl Plugin for SearchPlugin {
    const ID: &'static str = "search";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before SearchPlugin");
        registry.register(search());
    }
}
```

Registration must happen during `build()`. [`ToolsPlugin`](crate::tools::ToolsPlugin) freezes the
registry into a `GlobalResource` during `ready()`.

# Permission Model

| Level | Meaning |
|-------|---------|
| **Allow** (default) | Execute without confirmation |
| **Confirm** | Caller must obtain user confirmation |
| **Deny** | Reject execution entirely |

Override permissions at build time: `registry.set_permission("delete_file", ToolPermission::Deny)`.

# Execution

Inside a system, dispatch by name:

```ignore
# use polaris_ai::polaris_system;
use polaris_ai::system::{system, system::SystemError};
use polaris_ai::system::param::Res;
use polaris_ai::tools::ToolRegistry;

#[system]
async fn invoke_tool(registry: Res<ToolRegistry>) -> Result<serde_json::Value, SystemError> {
    registry.execute("search", &serde_json::json!({"query": "polaris"}))
        .await
        .map_err(|e| SystemError::ExecutionError(e.to_string()))
}
```

For LLM tool calling, pass `registry.definitions()` to the model provider
and dispatch returned calls through `registry.execute()`.

# Related

- [Model providers](crate::models) -- LLM integration that drives tool calling
- [Systems](crate::system) -- accessing `ToolRegistry` via `Res<ToolRegistry>`
