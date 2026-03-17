//! Anthropic provider plugin.

use super::provider::AnthropicProvider;
use polaris_models::{ModelRegistry, ModelsPlugin};
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;

/// Plugin providing support for Anthropic models.
///
/// ```no_run
/// # use polaris_model_providers::anthropic::AnthropicPlugin;
/// # use polaris_system::server::Server;
/// # let mut server = Server::new();
///
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

impl Plugin for AnthropicPlugin {
    const ID: &'static str = "polaris::provider::anthropic";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let provider = AnthropicProvider::new(self.api_key.clone());

        let Some(mut registry) = server.get_resource_mut::<ModelRegistry>() else {
            panic!(
                "ModelRegistry not found. Make sure to add ModelsPlugin before AnthropicPlugin."
            );
        };

        registry.register_llm_provider(provider);
    }
}
