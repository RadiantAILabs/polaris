//! AWS Bedrock provider plugin.

use super::provider::BedrockProvider;
use aws_sdk_bedrockruntime::Client;
use polaris_models::ModelRegistry;
use polaris_models::ModelsPlugin;
use polaris_system::plugin::{Plugin, PluginId, Version};
use polaris_system::server::Server;
use std::sync::Arc;

/// Plugin providing support for AWS Bedrock models.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(feature = "bedrock")]
/// # {
/// # use polaris_model_providers::BedrockPlugin;
/// # let mut server = polaris_system::server::Server::new();
///
/// // Using default AWS credential chain
/// server.add_plugins(BedrockPlugin::from_env());
///
/// // With custom SDK config
/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
/// let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
///     .region("us-west-2")
///     .load()
///     .await;
/// server.add_plugins(BedrockPlugin::from_sdk_config(sdk_config));
/// # });
/// }
/// ```
pub struct BedrockPlugin {
    sdk_config: Option<aws_config::SdkConfig>,
}

impl BedrockPlugin {
    /// Initialises [`BedrockPlugin`] using the default AWS credential chain.
    #[must_use]
    pub fn from_env() -> Self {
        Self { sdk_config: None }
    }

    /// Initialises [`BedrockPlugin`] from a pre-configured AWS SDK config.
    #[must_use]
    pub fn from_sdk_config(sdk_config: aws_config::SdkConfig) -> Self {
        Self {
            sdk_config: Some(sdk_config),
        }
    }
}

impl Default for BedrockPlugin {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Plugin for BedrockPlugin {
    const ID: &'static str = "polaris::provider::bedrock";
    const VERSION: Version = Version::new(0, 0, 1);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let sdk_config = match &self.sdk_config {
            Some(config) => config.clone(),
            None => std::thread::scope(|s| {
                s.spawn(|| {
                    let rt = tokio::runtime::Runtime::new()
                        .expect("failed to create tokio runtime for AWS config loading");
                    rt.block_on(aws_config::from_env().load())
                })
                .join()
                .expect("AWS config loading thread panicked")
            }),
        };

        let client = Client::new(&sdk_config);
        let provider = BedrockProvider::new(Arc::new(client));

        let Some(mut registry) = server.get_resource_mut::<ModelRegistry>() else {
            panic!("ModelRegistry not found. Make sure to add ModelsPlugin before BedrockPlugin.");
        };

        registry.register_llm_provider(provider);
    }
}
