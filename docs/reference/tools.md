---
notion_page: https://www.notion.so/radiant-ai/Tools-342afe2e695d80228a2ccc2130f85703
title: Tools
---

# Tools

`polaris_tools` provides the infrastructure for defining, registering, and executing LLM-callable tools. Tools are async functions with automatic JSON schema generation, parameter injection, and a permission model that controls invocation.

## Overview

| Primitive | Purpose |
|-----------|---------|
| `#[tool]` | Attribute macro turning an async function into a `Tool` impl |
| `#[toolset]` | Attribute macro generating `Toolset` for an `impl` block |
| `Tool` | Trait for executable tools (definition + `execute`) |
| `Toolset` | Trait for grouped tools on a struct |
| `ToolRegistry` | Stores tools by name, dispatches execution, tracks permissions |
| `ToolsPlugin` | Manages the registry lifecycle |
| `ToolPermission` | Per-tool access level (`Allow` / `Confirm` / `Deny`) |

## Setup

Register `ToolsPlugin` before any plugin that adds tools:

```rust
use polaris_tools::{ToolsPlugin, ToolRegistry};
use polaris_system::server::Server;

let mut server = Server::new();
server.add_plugins(ToolsPlugin);
server.add_plugins(SearchPlugin); // depends on ToolsPlugin
```

`ToolsPlugin` inserts a mutable `ToolRegistry` during `build()`. After all plugins have built, it freezes the registry into a `GlobalResource` in `ready()`. Tools must be registered during `build()`.

## Defining Tools

### Standalone Function

```rust
use polaris_tools::{tool, ToolError};

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

The macro:

1. Generates a struct `SearchTool` implementing `Tool`.
2. Generates a factory function `search()` returning an instance.
3. Extracts the doc comment as the tool description.
4. Extracts per-argument doc comments as parameter descriptions.
5. Builds a JSON schema from the argument types (via `schemars`).
6. Wires `#[default(expr)]` as the argument's default (making it optional in the schema).

Return type must be `Result<T, ToolError>` where `T: Serialize`. The returned value is serialized to `serde_json::Value` for the LLM.

### Toolsets (Grouped Tools)

When related tools share state or setup, group them on an `impl` block:

```rust
use polaris_tools::{toolset, tool, ToolError};

struct FileTools {
    root: std::path::PathBuf,
}

#[toolset]
impl FileTools {
    #[tool]
    /// List files in a directory relative to the root.
    async fn list_files(&self, path: String) -> Result<Vec<String>, ToolError> {
        // ... read self.root.join(path) ...
        Ok(vec![])
    }

    #[tool]
    /// Read a file's contents.
    async fn read_file(&self, path: String) -> Result<String, ToolError> {
        Ok(String::new())
    }
}
```

`#[toolset]` generates a `Toolset` impl whose `tools()` method yields one boxed `Tool` per `#[tool]` method. Register via `registry.register_toolset(FileTools { root })`.

### Manual `Tool` Impl

For dynamic tools whose schema is not known at compile time, implement `Tool` directly:

```rust
use polaris_tools::{Tool, ToolError, ToolPermission};
use polaris_models::llm::ToolDefinition;

struct DynamicQuery { schema: serde_json::Value }

impl Tool for DynamicQuery {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "dynamic_query".into(),
            description: "Run a dynamic query".into(),
            parameters: self.schema.clone(),
        }
    }

    fn permission(&self) -> ToolPermission { ToolPermission::Confirm }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, ToolError>> + Send + '_>> {
        Box::pin(async move { Ok(serde_json::json!({"ok": true})) })
    }
}
```

## Registration

Register inside a plugin's `build()` method, where `ToolRegistry` is a mutable resource:

```rust
use polaris_tools::ToolRegistry;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

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
            .expect("ToolsPlugin must be registered first");
        registry.register(search());
        registry.register_toolset(FileTools { root: "/tmp".into() });
    }
}
```

### Registration Rules

- Names must be unique. Registering a duplicate panics.
- Registration happens during `build()`. After `ready()`, the registry is a `GlobalResource` and cannot be mutated.
- Tool names are taken from the function identifier (snake_case).

## Permission Model

Every tool has a declared permission level:

| Level | Meaning |
|-------|---------|
| `Allow` (default) | Execute without user confirmation |
| `Confirm` | Caller must obtain user confirmation before execution |
| `Deny` | Reject execution entirely |

`Confirm` and `Deny` are advisory — `ToolRegistry::execute()` itself does not gate execution. Enforcement is the responsibility of the caller (typically the agent loop or a middleware).

### Declaring the Default

The `#[tool]` macro generates tools with `ToolPermission::Allow`. To declare a stricter default, implement `Tool` manually (see above) and override `fn permission(&self) -> ToolPermission`, or register the macro-generated tool and then narrow it via `registry.set_permission(...)` at build time.

### Runtime Overrides

During `build()`, configure per-tool overrides on the registry:

```rust
registry.set_permission("delete_file", ToolPermission::Deny)?;
registry.set_permission("search", ToolPermission::Allow)?;
```

Overrides allow both narrowing (`Allow` → `Deny`) and widening (`Deny` → `Allow`). Querying effective permission:

```rust
let effective: Option<ToolPermission> = registry.permission("delete_file");
```

Returns the override if set, otherwise the tool's declared default, or `None` if the tool is unknown.

## Execution Flow

Inside a system, obtain the registry via `Res<ToolRegistry>` and dispatch by name:

```rust
use polaris_tools::ToolRegistry;
use polaris_system::param::Res;
use polaris_system::system;

#[system]
async fn invoke_tool(
    registry: Res<ToolRegistry>,
) -> Result<serde_json::Value, SystemError> {
    let args = serde_json::json!({"query": "polaris", "limit": 5});
    registry
        .execute("search", &args)
        .await
        .map_err(|err| SystemError::ExecutionError(err.to_string()))
}
```

For LLM tool calling, pass `registry.definitions()` to the model provider and dispatch the returned tool calls through `registry.execute(&name, &args)`.

### Lookup

| Method | Returns |
|--------|---------|
| `registry.has(name)` | `bool` |
| `registry.get(name)` | `Option<&dyn Tool>` |
| `registry.to_arc(name)` | `Option<Arc<dyn Tool>>` (for decorators) |
| `registry.definitions()` | `Vec<ToolDefinition>` (for LLM tool lists) |
| `registry.names()` | `Vec<&str>` |

## Error Types

| Variant | Cause |
|---------|-------|
| `ToolError::UnknownTool(name)` | `execute()` called with an unregistered name |
| `ToolError::InvalidArguments(msg)` | Argument deserialization failed |
| `ToolError::ExecutionFailed(msg)` | Tool body returned an error |
| `ToolError::RegistryError(msg)` | Registry-level failure (e.g., `set_permission` on unknown tool) |

## Decorator Pattern

Plugins that wrap tools (e.g., a `TracingPlugin` that adds latency metrics) use the Arc-based accessor:

```rust
let names = registry.names().into_iter().map(str::to_string).collect::<Vec<_>>();
let mut new_registry = ToolRegistry::new();
for name in names {
    let original = registry.to_arc(&name).unwrap();
    new_registry.register(TracingWrapper::new(original));
}
// Preserve permission overrides
for (name, perm) in registry.permission_overrides() {
    new_registry.set_permission(name, *perm).ok();
}
```

Decorators must run during `build()` so they see the pre-freeze mutable registry.

## Key Files

| File | Purpose |
|------|---------|
| `polaris_tools/src/tool.rs` | `Tool` trait |
| `polaris_tools/src/toolset.rs` | `Toolset` trait |
| `polaris_tools/src/registry.rs` | `ToolRegistry`, `ToolsPlugin` |
| `polaris_tools/src/permission.rs` | `ToolPermission` enum |
| `polaris_tools/src/error.rs` | `ToolError` enum |
| `polaris_tools/src/schema.rs` | `FunctionMetadata`, `ParameterInfo` |
| `polaris_tools/tool_macros/src/lib.rs` | `#[tool]` / `#[toolset]` proc macros |
