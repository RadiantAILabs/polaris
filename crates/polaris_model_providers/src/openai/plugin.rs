//! `OpenAI` provider plugin.

use super::provider::OpenAiProvider;
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

/// Plugin providing support for `OpenAI` models via the Responses API.
///
/// Registers an [`OpenAiProvider`] with the [`ModelRegistry`] during
/// `build()`. After [`ModelsPlugin::ready`] freezes the registry, models
/// served by this provider become resolvable through
/// [`ModelRegistry::resolve`](polaris_models::ModelRegistry::resolve) under
/// the `openai/<model>` identifier.
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
///   registers itself with. Must be added before `OpenAiPlugin`.
///
/// # Lifecycle
///
/// - The plugin is gated behind the `openai` feature.
/// - **`build()`** — constructs an `OpenAiProvider` and registers it
///   with the [`ModelRegistry`], which is a mutable resource during the
///   `build()` phase. Panics if [`ModelsPlugin`] was not added first.
/// - No `ready()` or `cleanup()` behavior; registers no tick schedules.
///
/// # Extends
///
/// - [`ModelRegistry`] (from [`ModelsPlugin`]) — registers an
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

impl Plugin for OpenAiPlugin {
    const ID: &'static str = "polaris::provider::openai";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = OpenAiProvider::new(self.api_key.clone());

        let Some(mut registry) = server.get_resource_mut::<ModelRegistry>() else {
            panic!("ModelRegistry not found. Make sure to add ModelsPlugin before OpenAiPlugin.");
        };

        registry.register_llm_provider(provider);
    }
}
