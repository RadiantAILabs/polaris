//! Tool registry and plugin.
//!
//! The [`ToolRegistry`] stores registered tools and provides lookup/execution.
//! The [`ToolsPlugin`] manages the registry lifecycle using the two-phase
//! initialization pattern (mutable during `build()`, frozen to `GlobalResource`
//! in `ready()`).
//!
//! See the [crate-level documentation](crate) for a full usage example.

use crate::error::ToolError;
use crate::tool::Tool;
use crate::toolset::Toolset;
use indexmap::IndexMap;
use polaris_models::llm::ToolDefinition;
use polaris_system::plugin::{Plugin, Version};
use polaris_system::resource::GlobalResource;
use polaris_system::server::Server;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Registry of available tools.
///
/// Stores tools by name and provides lookup, execution, and definition listing.
#[derive(Default)]
pub struct ToolRegistry {
    tools: IndexMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl GlobalResource for ToolRegistry {}

impl ToolRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: IndexMap::new(),
        }
    }

    /// Registers a tool.
    ///
    /// # Panics
    ///
    /// Panics if a tool with the same name is already registered.
    pub fn register(&mut self, tool: impl Tool) {
        let name = tool.definition().name;
        assert!(
            !self.tools.contains_key(&name),
            "Tool '{name}' is already registered"
        );
        self.tools.insert(name, Arc::new(tool));
    }

    /// Registers all tools from a toolset.
    ///
    /// # Panics
    ///
    /// Panics if any tool name conflicts with an already-registered tool.
    pub fn register_toolset(&mut self, toolset: impl Toolset) {
        for tool in toolset.tools() {
            let name = tool.definition().name;
            assert!(
                !self.tools.contains_key(&name),
                "Tool '{name}' is already registered"
            );
            self.tools.insert(name, Arc::from(tool));
        }
    }

    /// Executes a tool by name with JSON arguments.
    pub fn execute<'a>(
        &'a self,
        name: &'a str,
        args: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ToolError>> + Send + 'a>> {
        let tool = self.tools.get(name).cloned();
        let args = args.clone();
        Box::pin(async move {
            let tool =
                tool.ok_or_else(|| ToolError::execution_error(format!("Unknown tool: {name}")))?;
            tool.execute(args).await
        })
    }

    /// Returns tool definitions for all registered tools.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// Returns a reference to a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// Returns whether a tool with the given name is registered.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Returns the names of all registered tools.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }
}

/// Plugin that provides the [`ToolRegistry`] global resource.
#[derive(Debug, Default, Clone)]
pub struct ToolsPlugin;

impl Plugin for ToolsPlugin {
    const ID: &'static str = "polaris::tools";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_resource(ToolRegistry::new());
    }

    fn ready(&self, server: &mut Server) {
        let registry = server
            .remove_resource::<ToolRegistry>()
            .expect("ToolRegistry should exist from build phase");
        server.insert_global(registry);
    }
}
