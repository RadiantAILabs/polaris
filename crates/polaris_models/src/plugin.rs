//! Provides the [`ModelRegistry`] global resource.

use crate::registry::ModelRegistry;
use polaris_system::plugin::{Plugin, Version};
use polaris_system::server::Server;

/// Plugin that provides the [`ModelRegistry`] for provider-agnostic model access.
///
/// # Lifecycle
///
/// The registry uses a two-phase initialization to allow provider registration while
/// ensuring immutability at runtime:
///
/// 1. **`build()` phase**: The registry is inserted as a mutable resource. Provider plugins
///    (e.g., `AnthropicPlugin`) access it via [`Server::get_resource_mut`] and call
///    [`ModelRegistry::register_llm_provider`] to register themselves.
///
/// 2. **`ready()` phase**: The registry is moved from a mutable resource to an immutable
///    global, ensuring thread-safe read-only access during agent execution.
///
/// # Usage
///
/// Add `ModelsPlugin` first, then add provider plugins which will register themselves.
///
/// Consumers can then obtain model handles via the registry using provider/model
/// identifiers (e.g., `"anthropic/claude-sonnet-4-20250514"`). See [`ModelRegistry`] for details.
#[derive(Debug, Default, Clone)]
pub struct ModelsPlugin;

impl Plugin for ModelsPlugin {
    const ID: &'static str = "polaris::models";
    const VERSION: Version = Version::new(0, 0, 1);

    fn build(&self, server: &mut Server) {
        server.insert_resource(ModelRegistry::new());
    }

    fn ready(&self, server: &mut Server) {
        let model_registry = server.remove_resource::<ModelRegistry>().unwrap();
        server.insert_global(model_registry);
    }
}
