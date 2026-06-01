//! Anthropic provider plugin.

use super::provider::AnthropicProvider;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

/// Plugin providing support for Anthropic models.
///
/// Registers an [`AnthropicProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`] freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::resolve`](polaris_models::ModelRegistry::resolve) under
/// the `anthropic/<model>` identifier.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only registers a provider with the [`ModelRegistry`] owned by [`ModelsPlugin`]. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin contributes a provider to [`ModelRegistry`] rather than installing its own API. |
///
/// # Dependencies
///
/// - [`ModelsPlugin`] — owns the [`ModelRegistry`] that this plugin
///   registers itself with. Must be added before `AnthropicPlugin`.
///
/// # Lifecycle
///
/// - The plugin is gated behind the `anthropic` feature.
/// - **`build()`** — constructs an `AnthropicProvider` and registers it
///   with the [`ModelRegistry`], which is a mutable resource during the
///   `build()` phase. Panics if [`ModelsPlugin`] was not added first.
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`]) — registers an
///   `AnthropicProvider` so that `anthropic/<model>` identifiers become
///   resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # use polaris_model_providers::anthropic::AnthropicPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"));
/// ```
pub struct AnthropicPlugin {
    api_key: String,
}

impl AnthropicPlugin {
    /// Creates a plugin that reads the API key from the specified environment variable.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable is not set.
    #[must_use]
    pub fn from_env(env_var: &str) -> Self {
        let api_key = std::env::var(env_var).unwrap_or_else(|_| {
            panic!("Environment variable {env_var} for AnthropicPlugin not set. Please set it to your Anthropic API key.");
        });
        Self { api_key }
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present — so the parameter is an infallible
// `&mut ModelRegistry` and the old "add ModelsPlugin first" panic is gone.
#[plugin(id = "polaris::provider::anthropic", version = "0.0.1")]
impl Plugin for AnthropicPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(AnthropicProvider::new(self.api_key.clone()));
    }
}
