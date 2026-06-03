//! `OpenAI` provider plugin.

use super::provider::OpenAiProvider;
use polaris_models::ModelRegistry;
use polaris_system::plugin;
use polaris_system::plugin::{Extends, Plugin};

/// Plugin providing support for `OpenAI` models via the Responses API.
///
/// Registers an [`OpenAiProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`](polaris_models::ModelsPlugin::ready) freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::llm`](polaris_models::ModelRegistry::llm) under
/// the `openai/<model>` identifier.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_   | —     | This plugin only registers a provider with the [`ModelRegistry`] owned by [`ModelsPlugin`](polaris_models::ModelsPlugin). |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin contributes a provider to [`ModelRegistry`] rather than installing its own API. |
///
/// # Dependencies
///
/// - [`ModelsPlugin`](polaris_models::ModelsPlugin) — owns the [`ModelRegistry`] that this plugin
///   registers itself with. The capability resolver orders `ModelsPlugin`
///   (the provider) before this plugin regardless of `add_plugins` order.
///
/// # Lifecycle
///
/// - The plugin is gated behind the `openai` feature.
/// - **`build()`** — constructs an `OpenAiProvider` and registers it
///   with the [`ModelRegistry`], which is a mutable resource during the
///   `build()` phase. The `Extends<ModelRegistry>` build param has the
///   resolver guarantee the registry is present and provider-ordered.
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`](polaris_models::ModelsPlugin)) — registers an
///   `OpenAiProvider` so that `openai/<model>` identifiers become
///   resolvable once `ModelsPlugin` freezes the registry in `ready()`.
///
/// # Example
///
/// ```no_run
/// # use polaris_model_providers::OpenAiPlugin;
/// # use polaris_models::ModelsPlugin;
/// # use polaris_system::server::Server;
/// let mut server = Server::new();
/// server.add_plugins(ModelsPlugin);
/// server.add_plugins(OpenAiPlugin::from_env("OPENAI_API_KEY"));
/// ```
pub struct OpenAiPlugin {
    api_key: String,
}

impl OpenAiPlugin {
    /// Creates a plugin that reads the API key from the specified environment variable.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable is not set.
    #[must_use]
    pub fn from_env(env_var: &str) -> Self {
        let api_key = std::env::var(env_var).unwrap_or_else(|_| {
            panic!("Environment variable {env_var} for OpenAiPlugin not set. Please set it to your OpenAI API key.");
        });
        Self { api_key }
    }
}

// The `Extends<ModelRegistry>` parameter is both the declaration (the macro derives
// `access().extends::<ModelRegistry>(...)` from it) and the access: the resolver orders
// this plugin after whichever plugin provides `ModelRegistry`, verifies the contract
// version, and guarantees the registry is present — so the parameter is an infallible
// `&mut ModelRegistry` and the old "add ModelsPlugin first" panic is gone.
#[plugin(id = "polaris::provider::openai", version = "0.0.1")]
impl Plugin for OpenAiPlugin {
    fn build(&self, mut registry: Extends<ModelRegistry>) {
        registry.register_llm_provider(OpenAiProvider::new(self.api_key.clone()));
    }
}
